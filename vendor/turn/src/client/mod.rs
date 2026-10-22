#[cfg(test)]
mod client_test;

pub mod binding;
pub mod integrity;
pub mod message;
pub mod permission;
pub mod relay_conn;
pub mod transaction;

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use binding::*;
use relay_conn::*;
use stun::agent::*;
use stun::attributes::*;
use stun::error_code::*;
use stun::fingerprint::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;
use stun::xoraddr::*;
use tokio::pin;
use tokio::select;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use transaction::*;
use util::conn::*;
use util::vnet::net::*;

use crate::error::*;
use crate::proto::chandata::*;
use crate::proto::data::*;
use crate::proto::lifetime::*;
use crate::proto::peeraddr::*;
use crate::proto::relayaddr::*;
use crate::proto::reqtrans::*;
use crate::proto::PROTO_UDP;

const MAX_DATA_BUFFER_SIZE: usize = 65_556; // Matches the async STUN/TCP socket buffer size
const MAX_READ_QUEUE_SIZE: usize = 1024;

/// The base socket transport, not the relay allocation's REQUESTED-TRANSPORT.
/// TURN/TLS is also a TCP stream; allocated peer traffic remains UDP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientTransport {
    #[default]
    Udp,
    Tcp,
}

/// ClientConfig is a bag of config parameters for Client.
pub struct ClientConfig {
    pub transport: ClientTransport,
    pub stun_serv_addr: String, // STUN server address (e.g. "stun.abc.com:3478")
    pub turn_serv_addr: String, // TURN server address (e.g. "turn.abc.com:3478")
    pub username: String,
    pub password: String,
    pub realm: String,
    pub software: String,
    pub rto_in_ms: u16,
    pub conn: Arc<dyn Conn + Send + Sync>,
    pub vnet: Option<Arc<Net>>,
}

struct ClientInternal {
    transport: ClientTransport,
    conn: Arc<dyn Conn + Send + Sync>,
    stun_serv_addr: String,
    turn_serv_addr: String,
    username: Username,
    password: String,
    realm: Realm,
    integrity: MessageIntegrity,
    allocation_nonce: Nonce,
    software: Software,
    transactions: Arc<TransactionManager>,
    binding_mgr: Arc<Mutex<BindingManager>>,
    rto_in_ms: u16,
    read_ch_tx: Arc<Mutex<Option<mpsc::Sender<InboundData>>>>,
    close_notify: CancellationToken,
    read_task: Option<tokio::task::JoinHandle<()>>,
    transport_failure: Arc<StdMutex<Option<String>>>,
}

impl Drop for ClientInternal {
    fn drop(&mut self) {
        self.close_notify.cancel();
        // Explicit close awaits the reader. Construction cancellation/unwind
        // must also stop it rather than detach a live socket reader.
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
    }
}

#[async_trait]
impl RelayConnObserver for ClientInternal {
    /// Returns the TURN server address.
    fn turn_server_addr(&self) -> String {
        self.turn_serv_addr.clone()
    }

    /// Returns the `username`.
    fn username(&self) -> Username {
        self.username.clone()
    }

    /// Return the `realm`.
    fn realm(&self) -> Realm {
        self.realm.clone()
    }

    fn set_realm(&mut self, realm: Realm) {
        // A realm configured up front still needs its key.
        if self.realm.text == realm.text && !self.integrity.0.is_empty() {
            return;
        }
        // The key is re-derived when the realm changes. In-flight
        // transactions have already captured their own signing key.
        self.integrity = MessageIntegrity::new_long_term_integrity(
            self.username.text.clone(),
            realm.text.clone(),
            self.password.clone(),
        );
        self.realm = realm;
    }

    fn integrity(&self) -> MessageIntegrity {
        self.integrity.clone()
    }

    async fn close_allocation_data(&mut self) {
        self.read_ch_tx.lock().await.take();
    }

    async fn close_transport(&self) -> std::result::Result<(), util::Error> {
        self.conn.close().await
    }

    /// Sends data to the specified destination using the base socket.
    async fn write_to(&self, data: &[u8], to: &str) -> std::result::Result<usize, util::Error> {
        if self.close_notify.is_cancelled() {
            return Err(util::Error::Other("TURN client closed".to_owned()));
        }
        let destination = SocketAddr::from_str(to)?;
        let n = select! {
            biased;
            _ = self.close_notify.cancelled() => {
                return Err(util::Error::Other("TURN client closed".to_owned()));
            }
            result = self.conn.send_to(data, destination) => result?,
        };
        Ok(n)
    }

    /// Performs STUN transaction.
    fn transaction_after(
        &self,
        msg: &Message,
        to: &str,
        ignore_result: bool,
        delay: std::time::Duration,
    ) -> Result<Transaction> {
        self.transactions.start_after(
            Arc::clone(&self.conn),
            msg,
            SocketAddr::from_str(to)?,
            msg.contains(ATTR_MESSAGE_INTEGRITY)
                .then(|| self.integrity.clone()),
            self.rto_in_ms,
            ignore_result,
            delay,
        )
    }
}

impl ClientInternal {
    /// Creates a new [`ClientInternal`].
    async fn new(config: ClientConfig) -> Result<Self> {
        let net = if let Some(vnet) = config.vnet {
            if vnet.is_virtual() {
                log::warn!("vnet is enabled");
            }
            vnet
        } else {
            Arc::new(Net::new(None))
        };

        let stun_serv_addr = if config.stun_serv_addr.is_empty() {
            String::new()
        } else {
            log::debug!("resolving {}", config.stun_serv_addr);
            let local_addr = config.conn.local_addr()?;
            let stun_serv = net
                .resolve_addr(local_addr.is_ipv4(), &config.stun_serv_addr)
                .await?;
            log::debug!("stunServ: {stun_serv}");
            stun_serv.to_string()
        };

        let turn_serv_addr = if config.turn_serv_addr.is_empty() {
            String::new()
        } else {
            log::debug!("resolving {}", config.turn_serv_addr);
            let local_addr = config.conn.local_addr()?;
            let turn_serv = net
                .resolve_addr(local_addr.is_ipv4(), &config.turn_serv_addr)
                .await?;
            log::debug!("turnServ: {turn_serv}");
            turn_serv.to_string()
        };

        let close_notify = CancellationToken::new();
        Ok(ClientInternal {
            transport: config.transport,
            conn: Arc::clone(&config.conn),
            stun_serv_addr,
            turn_serv_addr,
            username: Username::new(ATTR_USERNAME, config.username),
            password: config.password,
            realm: Realm::new(ATTR_REALM, config.realm),
            software: Software::new(ATTR_SOFTWARE, config.software),
            transactions: Arc::new(TransactionManager::new(close_notify.clone())),
            binding_mgr: Arc::new(Mutex::new(BindingManager::new())),
            rto_in_ms: if config.rto_in_ms != 0 {
                config.rto_in_ms
            } else {
                DEFAULT_RTO_MS
            },
            integrity: MessageIntegrity::new_short_term_integrity(String::new()),
            allocation_nonce: Nonce::new(ATTR_NONCE, String::new()),
            read_ch_tx: Arc::new(Mutex::new(None)),
            close_notify,
            read_task: None,
            transport_failure: Arc::new(StdMutex::new(None)),
        })
    }

    /// Returns the STUN server address.
    fn stun_server_addr(&self) -> String {
        self.stun_serv_addr.clone()
    }

    /// Start one owned transport reader. Packet errors do not end the allocation.
    async fn listen(&mut self) -> Result<()> {
        if self.close_notify.is_cancelled() {
            return Err(Error::Other("TURN client is closed".to_owned()));
        }
        if self.read_task.is_some() {
            return Err(Error::Other(
                "TURN transport reader already started".to_owned(),
            ));
        }
        let conn = Arc::clone(&self.conn);
        let stun_serv_str = self.stun_serv_addr.clone();
        let turn_serv_str = self.turn_serv_addr.clone();
        let transactions = Arc::clone(&self.transactions);
        let read_ch_tx = Arc::clone(&self.read_ch_tx);
        let binding_mgr = Arc::clone(&self.binding_mgr);
        let close_notify = self.close_notify.clone();
        let transport_failure = self.transport_failure.clone();
        let transport = self.transport;

        self.read_task = Some(tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATA_BUFFER_SIZE];
            let wait_cancel = close_notify.cancelled();
            pin!(wait_cancel);

            let failure = loop {
                let (n, from, received_at) = select! {
                    biased;

                    _ = &mut wait_cancel => {
                        log::debug!("exiting read loop");
                        break None;
                    },
                    result = conn.recv_from_with_timestamp(&mut buf) => match result {
                        Ok(packet) => packet,
                        Err(err) if transport == ClientTransport::Udp => {
                            // A datagram receive error is logged and returns to
                            // the event loop; it does not close the port.
                            log::debug!("TURN UDP receive error: {err}");
                            tokio::task::yield_now().await;
                            continue;
                        }
                        Err(err) => {
                            break Some(err.to_string());
                        }
                    }
                };
                log::debug!("received {n} TURN transport bytes from {from}");

                select! {
                    biased;

                    _ = &mut wait_cancel => {
                        log::debug!("exiting read loop");
                        break None;
                    },
                    result = ClientInternal::handle_inbound(
                        &read_ch_tx,
                        &buf[..n],
                        from,
                        &stun_serv_str,
                        &turn_serv_str,
                        &transactions,
                        &binding_mgr,
                        received_at,
                    ) => {
                        if let Err(err) = result {
                            // Malformed messages are dropped, not the port.
                            log::warn!("discarded invalid TURN packet from {from}: {err}");
                        }
                    }
                }
            };
            if let Some(error) = failure {
                // A socket failure clears requests and destroys its
                // Connections immediately, not after ICE timeout.
                log::warn!("TURN transport reader failed: {error}");
                transport_failure
                    .lock()
                    .expect("TURN transport failure mutex poisoned")
                    .get_or_insert_with(|| format!("TURN transport failed: {error}"));
                close_notify.cancel();
                read_ch_tx.lock().await.take();
                transactions.close().await;
                if let Err(error) = conn.close().await {
                    log::debug!("closing failed TURN transport: {error}");
                }
            }
        }));

        Ok(())
    }

    /// Demultiplex this client's transport. Errors discard one packet; they do
    /// not stop the reader. Unrecognized packets are not relay application data.
    async fn handle_inbound(
        read_ch_tx: &Arc<Mutex<Option<mpsc::Sender<InboundData>>>>,
        data: &[u8],
        from: SocketAddr,
        stun_serv_str: &str,
        turn_serv_str: &str,
        transactions: &Arc<TransactionManager>,
        binding_mgr: &Arc<Mutex<BindingManager>>,
        received_at: std::time::Instant,
    ) -> Result<()> {
        if is_message(data) {
            ClientInternal::handle_stun_message(
                transactions,
                read_ch_tx,
                data,
                from,
                turn_serv_str,
                received_at,
            )
            .await
        } else if ChannelData::is_channel_data(data) {
            if from.to_string() == turn_serv_str {
                ClientInternal::handle_channel_data(binding_mgr, read_ch_tx, data, received_at)
                    .await
            } else {
                Ok(())
            }
        } else if !stun_serv_str.is_empty() && from.to_string() == *stun_serv_str {
            // received from STUN server but it is not a STUN message
            Err(Error::ErrNonStunmessage)
        } else {
            log::trace!("non-STUN/TURN packet, unhandled");
            Ok(())
        }
    }

    async fn handle_stun_message(
        transactions: &Arc<TransactionManager>,
        read_ch_tx: &Arc<Mutex<Option<mpsc::Sender<InboundData>>>>,
        data: &[u8],
        mut from: SocketAddr,
        turn_serv_str: &str,
        received_at: std::time::Instant,
    ) -> Result<()> {
        let msg = message::decode(data, message::Dialect::Turn)?;

        if msg.typ.class == CLASS_REQUEST {
            return Err(Error::Other(format!(
                "{:?} : {}",
                Error::ErrUnexpectedStunrequestMessage,
                msg
            )));
        }

        if msg.typ.class == CLASS_INDICATION {
            if msg.typ.method == METHOD_DATA && from.to_string() == turn_serv_str {
                let mut peer_addr = PeerAddress::default();
                peer_addr.get_from(&msg)?;
                from = SocketAddr::new(peer_addr.ip, peer_addr.port);

                let mut data = Data::default();
                data.get_from(&msg)?;

                log::debug!("data indication received from {from}");

                let _ = ClientInternal::handle_inbound_relay_conn(
                    read_ch_tx,
                    &data.0,
                    from,
                    received_at,
                )
                .await;
            }

            return Ok(());
        }

        // This is a STUN response message (transactional)
        // The type is either:
        // - stun.ClassSuccessResponse
        // - stun.ClassErrorResponse

        transactions.handle_response(msg, from);

        Ok(())
    }

    async fn handle_channel_data(
        binding_mgr: &Arc<Mutex<BindingManager>>,
        read_ch_tx: &Arc<Mutex<Option<mpsc::Sender<InboundData>>>>,
        data: &[u8],
        received_at: std::time::Instant,
    ) -> Result<()> {
        let mut ch_data = ChannelData {
            raw: data.to_vec(),
            ..Default::default()
        };
        ch_data.decode()?;

        let addr = ClientInternal::find_addr_by_channel_number(binding_mgr, ch_data.number.0)
            .await
            .ok_or(Error::ErrChannelBindNotFound)?;

        log::trace!(
            "channel data received from {} (ch={})",
            addr,
            ch_data.number.0
        );

        let _ =
            ClientInternal::handle_inbound_relay_conn(read_ch_tx, &ch_data.data, addr, received_at)
                .await;

        Ok(())
    }

    /// Passes inbound data in RelayConn.
    async fn handle_inbound_relay_conn(
        read_ch_tx: &Arc<Mutex<Option<mpsc::Sender<InboundData>>>>,
        data: &[u8],
        from: SocketAddr,
        received_at: std::time::Instant,
    ) -> Result<()> {
        let read_ch_tx_opt = read_ch_tx.lock().await;
        log::debug!("read_ch_tx_opt = {}", read_ch_tx_opt.is_some());
        if let Some(tx) = &*read_ch_tx_opt {
            log::debug!("try_send data = {data:?}, from = {from}");
            if tx
                .try_send(InboundData {
                    data: data.to_vec(),
                    from,
                    received_at,
                })
                .is_err()
            {
                log::warn!("receive buffer full");
            }
            Ok(())
        } else {
            Err(Error::ErrAlreadyClosed)
        }
    }

    /// Closes this client.
    async fn close(&mut self, close_transport: bool) {
        self.close_notify.cancel();
        {
            let mut read_ch_tx = self.read_ch_tx.lock().await;
            read_ch_tx.take();
        }
        self.transactions.close().await;
        // The reader only borrows cloned state maps, never client_internal.
        // Do not hold a transaction/data-queue lock across its completion.
        if let Some(task) = self.read_task.take() {
            if let Err(error) = task.await {
                log::error!("TURN transport reader failed during shutdown: {error}");
            } else {
                log::debug!("TURN transport reader joined");
            }
        }
        if close_transport {
            if let Err(error) = self.conn.close().await {
                log::debug!("closing TURN transport: {error}");
            }
        }
    }

    /// Sends a new STUN request to the given transport address.
    async fn send_binding_request_to(&mut self, to: &str) -> Result<SocketAddr> {
        let msg = {
            let attrs: Vec<Box<dyn Setter>> = if !self.software.text.is_empty() {
                vec![
                    Box::new(TransactionId::new()),
                    Box::new(BINDING_REQUEST),
                    Box::new(self.software.clone()),
                ]
            } else {
                vec![Box::new(TransactionId::new()), Box::new(BINDING_REQUEST)]
            };

            let mut msg = Message::new();
            msg.build(&attrs)?;
            msg
        };

        log::debug!("client.SendBindingRequestTo call PerformTransaction 1");
        let tr_res = self.transaction(&msg, to, false)?.wait().await?;

        let mut refl_addr = XorMappedAddress::default();
        refl_addr.get_from(&tr_res.msg)?;

        Ok(SocketAddr::new(refl_addr.ip, refl_addr.port))
    }

    /// Sends a new STUN request to the STUN server.
    async fn send_binding_request(&mut self) -> Result<SocketAddr> {
        if self.stun_serv_addr.is_empty() {
            Err(Error::ErrStunserverAddressNotSet)
        } else {
            self.send_binding_request_to(&self.stun_serv_addr.clone())
                .await
        }
    }

    /// Returns a peer address associated with the
    // channel number on this UDPConn
    async fn find_addr_by_channel_number(
        binding_mgr: &Arc<Mutex<BindingManager>>,
        ch_num: u16,
    ) -> Option<SocketAddr> {
        let bm = binding_mgr.lock().await;
        bm.find_by_number(ch_num).map(|b| b.addr)
    }

    async fn wait_for_allocation_close(&self, reason: &str) -> Result<RelayConnConfig> {
        log::warn!("TURN Allocate response did not complete allocation: {reason}");
        self.close_notify.cancelled().await;
        Err(Error::ErrTransactionClosed)
    }

    /// Sends a TURN allocation request to the given transport address.
    async fn allocate(&mut self) -> Result<RelayConnConfig> {
        {
            let read_ch_tx = self.read_ch_tx.lock().await;
            log::debug!("allocate check: read_ch_tx_opt = {}", read_ch_tx.is_some());
            if read_ch_tx.is_some() {
                return Err(Error::ErrOneAllocateOnly);
            }
        }

        let (relayed_addr, lifetime) = loop {
            let request = {
                let mut attributes: Vec<Box<dyn Setter>> = vec![
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
                    Box::new(RequestedTransport {
                        protocol: PROTO_UDP,
                    }),
                ];
                if !self.integrity.0.is_empty() {
                    attributes.extend([
                        Box::new(self.username.clone()) as Box<dyn Setter>,
                        Box::new(self.realm.clone()),
                        Box::new(self.allocation_nonce.clone()),
                        Box::new(self.integrity.clone()),
                    ]);
                }
                attributes.push(Box::new(FINGERPRINT));
                let mut request = Message::new();
                request.build(&attributes)?;
                request
            };
            let response = self
                .transaction(&request, &self.turn_serv_addr, false)?
                .wait()
                .await?
                .msg;
            if response.typ.class == CLASS_ERROR_RESPONSE {
                let mut error = ErrorCodeAttribute::default();
                let (code, reason) = match error.get_from(&response) {
                    Ok(()) => (
                        error.code.0,
                        String::from_utf8_lossy(&error.reason).into_owned(),
                    ),
                    Err(_) => (0, String::new()),
                };
                match code {
                    401 if self.integrity.0.is_empty() => {
                        let Ok(realm) = Realm::get_from_as(&response, ATTR_REALM) else {
                            return self.wait_for_allocation_close("401 missing realm").await;
                        };
                        self.set_realm(realm);
                        let Ok(nonce) = Nonce::get_from_as(&response, ATTR_NONCE) else {
                            return self.wait_for_allocation_close("401 missing nonce").await;
                        };
                        self.allocation_nonce = nonce;
                    }
                    300 => {
                        let mut alternate = stun::addr::MappedAddress::default();
                        if alternate
                            .get_from_as(&response, ATTR_ALTERNATE_SERVER)
                            .is_err()
                        {
                            return Err(Error::AllocationRejected { code, reason });
                        }
                        let realm = if response.contains(ATTR_REALM) {
                            Some(Realm::get_from_as(&response, ATTR_REALM)?.text)
                        } else {
                            None
                        };
                        let nonce = if response.contains(ATTR_NONCE) {
                            Some(Nonce::get_from_as(&response, ATTR_NONCE)?.text)
                        } else {
                            None
                        };
                        return Err(Error::AllocationRedirect {
                            target: SocketAddr::new(alternate.ip, alternate.port),
                            realm,
                            nonce,
                        });
                    }
                    437 => return Err(Error::AllocationMismatch),
                    _ => return Err(Error::AllocationRejected { code, reason }),
                }
                continue;
            }
            let mut mapped = XorMappedAddress::default();
            let mut relayed = RelayedAddress::default();
            let mut lifetime = Lifetime::default();
            if mapped.get_from(&response).is_err()
                || relayed.get_from(&response).is_err()
                || lifetime.get_from(&response).is_err()
            {
                // An incomplete success stops this request chain. It does
                // not publish an allocation or immediately invent a new retry.
                return self
                    .wait_for_allocation_close("success missing mapped/relayed/lifetime")
                    .await;
            }
            break (SocketAddr::new(relayed.ip, relayed.port), lifetime);
        };
        let nonce = self.allocation_nonce.clone();

        let (read_ch_tx, read_ch_rx) = mpsc::channel(MAX_READ_QUEUE_SIZE);
        {
            let mut read_ch_tx_opt = self.read_ch_tx.lock().await;
            // Serialize installation with the reader's failure cleanup. EOF can
            // arrive immediately after a valid Allocate response; never recreate
            // an allocation data sender after that cleanup has already run.
            if self.close_notify.is_cancelled() {
                return Err(Error::ErrTransactionClosed);
            }
            *read_ch_tx_opt = Some(read_ch_tx);
            log::debug!("allocate: read_ch_tx_opt = {}", read_ch_tx_opt.is_some());
        }

        Ok(RelayConnConfig {
            relayed_addr,
            nonce,
            lifetime: lifetime.0,
            binding_mgr: Arc::clone(&self.binding_mgr),
            read_ch_rx: Arc::new(Mutex::new(read_ch_rx)),
            client_closed: self.close_notify.clone(),
            allocation_failure_reason: self.transport_failure.clone(),
        })
    }
}

/// Client is a STUN server client.
#[derive(Clone)]
pub struct Client {
    client_internal: Arc<Mutex<ClientInternal>>,
    close_notify: CancellationToken,
    ingress: Arc<ClientIngress>,
}

/// Authentication inherited by a validated Allocate 300 redirect. A realm
/// attribute may legitimately be empty; signing is not inferred from length.
#[derive(Clone, Default)]
pub struct AllocationAuth {
    pub realm: String,
    pub nonce: String,
    pub signed: bool,
}

/// Shared UDP AllocationSequence ingress. It borrows transaction/channel state
/// without locking ClientInternal, which may be awaiting Allocate completion.
/// The caller owns the physical reader; it must not also call Client::listen.
pub struct ClientIngress {
    remote: Option<SocketAddr>,
    transactions: Arc<TransactionManager>,
    binding_mgr: Arc<Mutex<BindingManager>>,
    read_ch_tx: Arc<Mutex<Option<mpsc::Sender<InboundData>>>>,
    closed: CancellationToken,
}

impl ClientIngress {
    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote
    }
    pub fn matches_source(&self, source: SocketAddr) -> bool {
        self.remote == Some(source)
    }

    pub async fn handle_shared_packet(
        &self,
        data: &[u8],
        source: SocketAddr,
        received_at: std::time::Instant,
    ) -> Result<bool> {
        if !self.matches_source(source) || self.closed.is_cancelled() || data.len() < 4 {
            return Ok(false);
        }
        let typ = u16::from_be_bytes([data[0], data[1]]);
        // Shared-socket Binding responses are left to the STUN manager, even
        // when a TURN server has the same address.
        if typ == 0x0101 || typ == 0x0111 {
            return Ok(false);
        }
        ClientInternal::handle_inbound(
            &self.read_ch_tx,
            data,
            source,
            "",
            &source.to_string(),
            &self.transactions,
            &self.binding_mgr,
            received_at,
        )
        .await?;
        Ok(true)
    }
}

impl Client {
    pub async fn new(config: ClientConfig) -> Result<Self> {
        let ci = ClientInternal::new(config).await?;
        let ingress = Arc::new(ClientIngress {
            remote: if ci.turn_serv_addr.is_empty() {
                None
            } else {
                Some(ci.turn_serv_addr.parse()?)
            },
            transactions: ci.transactions.clone(),
            binding_mgr: ci.binding_mgr.clone(),
            read_ch_tx: ci.read_ch_tx.clone(),
            closed: ci.close_notify.clone(),
        });
        Ok(Client {
            close_notify: ci.close_notify.clone(),
            client_internal: Arc::new(Mutex::new(ci)),
            ingress,
        })
    }

    pub fn shared_ingress(&self) -> Arc<ClientIngress> {
        self.ingress.clone()
    }

    pub async fn allocation_auth(&self) -> AllocationAuth {
        let ci = self.client_internal.lock().await;
        AllocationAuth {
            realm: ci.realm.text.clone(),
            nonce: ci.allocation_nonce.text.clone(),
            signed: !ci.integrity.0.is_empty(),
        }
    }

    pub async fn set_allocation_auth(&self, auth: AllocationAuth) {
        let mut ci = self.client_internal.lock().await;
        ci.allocation_nonce = Nonce::new(ATTR_NONCE, auth.nonce);
        if auth.signed {
            ci.realm = Realm::new(ATTR_REALM, auth.realm);
            ci.integrity = MessageIntegrity::new_long_term_integrity(
                ci.username.text.clone(),
                ci.realm.text.clone(),
                ci.password.clone(),
            );
        } else {
            ci.realm = Realm::new(ATTR_REALM, auth.realm);
            ci.integrity = MessageIntegrity::new_short_term_integrity(String::new());
        }
    }

    pub async fn listen(&self) -> Result<()> {
        let mut ci = self.client_internal.lock().await;
        ci.listen().await
    }

    pub async fn allocate(&self) -> Result<impl Conn> {
        let config = {
            let mut ci = self.client_internal.lock().await;
            ci.allocate().await?
        };

        RelayConn::new(Arc::clone(&self.client_internal), config).await
    }

    pub async fn close(&self) -> Result<()> {
        // Allocation may be waiting while holding client_internal. Wake it
        // before acquiring that lock; never wait 39.75 seconds to close.
        self.close_notify.cancel();
        let mut ci = self.client_internal.lock().await;
        ci.close(true).await;
        Ok(())
    }

    /// Allocate's UDP 300 transition stops the old protocol reader/requests,
    /// but preserves the same bound socket for the alternate server.
    pub async fn detach_transport(&self) {
        self.close_notify.cancel();
        self.client_internal.lock().await.close(false).await;
    }

    /// Sends a new STUN request to the given transport address.
    pub async fn send_binding_request_to(&self, to: &str) -> Result<SocketAddr> {
        let mut ci = self.client_internal.lock().await;
        ci.send_binding_request_to(to).await
    }

    /// Sends a new STUN request to the STUN server.
    pub async fn send_binding_request(&self) -> Result<SocketAddr> {
        let mut ci = self.client_internal.lock().await;
        ci.send_binding_request().await
    }
}
