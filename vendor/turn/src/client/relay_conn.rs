// client implements the API for a TURN client
use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use async_trait::async_trait;
use stun::agent::*;
use stun::attributes::*;
use stun::error_code::*;
use stun::fingerprint::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use util::Conn;

use super::binding::*;
use super::permission::*;
use super::transaction::*;
use crate::{proto, Error};

const TURN_ENTRY_REFRESH_INTERVAL: Duration = Duration::from_secs(240);
const MIN_TIMER_INTERVAL: Duration = Duration::from_millis(1);

fn allocation_refresh_delay(lifetime: Duration) -> Duration {
    let seconds = lifetime.as_secs();
    if seconds <= 120 {
        lifetime / 2
    } else if seconds <= 3_600 {
        lifetime.saturating_sub(Duration::from_secs(60))
    } else {
        Duration::from_secs(3_540)
    }
    .max(MIN_TIMER_INTERVAL)
}

pub(crate) struct InboundData {
    pub(crate) data: Vec<u8>,
    pub(crate) from: SocketAddr,
    pub(crate) received_at: Instant,
}

/// `RelayConnObserver` is an interface to [`RelayConn`] observer.
#[async_trait]
pub trait RelayConnObserver {
    fn turn_server_addr(&self) -> String;
    fn username(&self) -> Username;
    fn realm(&self) -> Realm;
    fn set_realm(&mut self, realm: Realm);
    fn integrity(&self) -> MessageIntegrity;
    fn auth_setters(&self, nonce: &Nonce) -> Vec<Box<dyn Setter>> {
        let integrity = self.integrity();
        if integrity.0.is_empty() {
            return Vec::new();
        }
        vec![
            Box::new(self.username()),
            Box::new(self.realm()),
            Box::new(nonce.clone()),
            Box::new(integrity),
        ]
    }
    async fn close_allocation_data(&mut self);
    async fn close_transport(&self) -> Result<(), util::Error>;
    async fn write_to(&self, data: &[u8], to: &str) -> Result<usize, util::Error>;
    fn transaction(
        &self,
        msg: &Message,
        to: &str,
        ignore_result: bool,
    ) -> Result<Transaction, Error> {
        self.transaction_after(msg, to, ignore_result, Duration::ZERO)
    }
    fn transaction_after(
        &self,
        msg: &Message,
        to: &str,
        ignore_result: bool,
        delay: Duration,
    ) -> Result<Transaction, Error>;
}

/// `RelayConnConfig` is a set of configuration params used by [`RelayConn::new()`].
pub(crate) struct RelayConnConfig {
    pub(crate) relayed_addr: SocketAddr,
    pub(crate) nonce: Nonce,
    pub(crate) lifetime: Duration,
    pub(crate) binding_mgr: Arc<Mutex<BindingManager>>,
    pub(crate) read_ch_rx: Arc<Mutex<mpsc::Receiver<InboundData>>>,
    pub(crate) client_closed: CancellationToken,
    pub(crate) allocation_failure_reason: Arc<StdMutex<Option<String>>>,
}

pub struct RelayConnInternal<T: 'static + RelayConnObserver + Send + Sync> {
    obs: Arc<Mutex<T>>,
    relayed_addr: SocketAddr,
    perm_map: PermissionMap,
    binding_mgr: Arc<Mutex<BindingManager>>,
    nonce: Arc<StdMutex<Nonce>>,
    allocation_failed: bool,
    allocation_failure_reason: Arc<StdMutex<Option<String>>>,
    failed_peer_addrs: Arc<StdMutex<HashSet<SocketAddr>>>,
    request_tasks: TaskTracker,
    cancel: CancellationToken,
}

/// `RelayConn` is the implementation of the Conn interfaces for UDP Relayed network connections.
pub struct RelayConn<T: 'static + RelayConnObserver + Send + Sync> {
    relayed_addr: SocketAddr,
    read_ch_rx: Arc<Mutex<mpsc::Receiver<InboundData>>>,
    relay_conn: Arc<Mutex<RelayConnInternal<T>>>,
    allocation_failure_reason: Arc<StdMutex<Option<String>>>,
    failed_peer_addrs: Arc<StdMutex<HashSet<SocketAddr>>>,
    request_tasks: TaskTracker,
    cancel: CancellationToken,
}

impl<T: RelayConnObserver + Send + Sync> Drop for RelayConn<T> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl<T: 'static + RelayConnObserver + Send + Sync> RelayConn<T> {
    /// Creates a new [`RelayConn`].
    pub(crate) async fn new(obs: Arc<Mutex<T>>, config: RelayConnConfig) -> Result<Self, Error> {
        log::debug!("initial lifetime: {} seconds", config.lifetime.as_secs());
        let allocation_interval = allocation_refresh_delay(config.lifetime);
        let allocation_failure_reason = config.allocation_failure_reason.clone();
        let failed_peer_addrs = Arc::new(StdMutex::new(HashSet::new()));
        let request_tasks = TaskTracker::new();
        let cancel = config.client_closed.child_token();

        let c = RelayConn {
            relayed_addr: config.relayed_addr,
            read_ch_rx: Arc::clone(&config.read_ch_rx),
            relay_conn: Arc::new(Mutex::new(RelayConnInternal::new(
                obs,
                config,
                Arc::clone(&allocation_failure_reason),
                Arc::clone(&failed_peer_addrs),
                request_tasks.clone(),
                cancel.clone(),
            ))),
            allocation_failure_reason,
            failed_peer_addrs,
            request_tasks,
            cancel,
        };

        let (obs, nonce) = {
            let state = c.relay_conn.lock().await;
            (state.obs.clone(), state.nonce.clone())
        };
        let transaction = RelayConnInternal::begin_allocation_refresh(
            &obs,
            &nonce,
            None,
            false,
            allocation_interval,
        )
        .await?;
        let weak = Arc::downgrade(&c.relay_conn);
        let cancel = c.cancel.clone();
        c.request_tasks.spawn(async move {
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                result = RelayConnInternal::allocation_refresh_chain(&obs, &nonce, transaction) => result,
            };
            let Err(error) = result else { return; };
            let Some(state) = weak.upgrade() else { return; };
            let mut state = state.lock().await;
            state.allocation_failed = true;
            *state.allocation_failure_reason.lock().expect("TURN allocation failure mutex poisoned") = Some(error.to_string());
            drop(state);
            log::warn!("TURN allocation ended: {error}");
            let mut obs = obs.lock().await;
            obs.close_allocation_data().await;
            if let Err(error) = obs.close_transport().await { log::warn!("closing TURN transport failed: {error}"); }
        });
        Ok(c)
    }

    async fn send_packet(
        &self,
        p: &[u8],
        addr: SocketAddr,
        payload: bool,
    ) -> Result<usize, util::Error> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(util::Error::Other("TURN allocation closed".into())),
            result = async {
                self.relay_conn.lock().await.send_to(p, addr, payload).await
                    .map_err(|error| util::Error::Other(error.to_string()))
            } => result,
        }
    }
}

#[async_trait]
impl<T: RelayConnObserver + Send + Sync> Conn for RelayConn<T> {
    async fn connect(&self, _addr: SocketAddr) -> Result<(), util::Error> {
        Err(io::Error::other("Not applicable").into())
    }

    async fn recv(&self, _buf: &mut [u8]) -> Result<usize, util::Error> {
        Err(io::Error::other("Not applicable").into())
    }

    /// Reads a packet from the connection,
    /// copying the payload into `p`. It returns the number of
    /// bytes copied into `p` and the return address that
    /// was on the packet.
    /// It returns the number of bytes read `(0 <= n <= len(p))`
    /// and any error encountered. Callers should always process
    /// the `n > 0` bytes returned before considering the error.
    /// It can be made to time out and return
    /// an Error with Timeout() == true after a fixed time limit;
    /// see SetDeadline and SetReadDeadline.
    async fn recv_from(&self, p: &mut [u8]) -> Result<(usize, SocketAddr), util::Error> {
        let (n, from, _) = self.recv_from_with_timestamp(p).await?;
        Ok((n, from))
    }

    async fn recv_from_with_timestamp(
        &self,
        p: &mut [u8],
    ) -> Result<(usize, SocketAddr, Instant), util::Error> {
        let mut read_ch_rx = self.read_ch_rx.lock().await;

        let data = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => None,
            data = read_ch_rx.recv() => data,
        };
        if let Some(ib_data) = data {
            let n = ib_data.data.len();
            if p.len() < n {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    Error::ErrShortBuffer.to_string(),
                )
                .into());
            }
            p[..n].copy_from_slice(&ib_data.data);
            Ok((n, ib_data.from, ib_data.received_at))
        } else {
            let reason = self
                .allocation_failure_reason
                .lock()
                .expect("TURN allocation failure mutex poisoned")
                .clone()
                .unwrap_or_else(|| Error::ErrAlreadyClosed.to_string());
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, reason).into())
        }
    }

    async fn send(&self, _buf: &[u8]) -> Result<usize, util::Error> {
        Err(io::Error::other("Not applicable").into())
    }

    /// Writes a packet with payload `p` to `addr`.
    /// It can be made to time out and return
    /// an Error with Timeout() == true after a fixed time limit;
    /// see SetDeadline and SetWriteDeadline.
    /// On packet-oriented connections, write timeouts are rare.
    async fn send_to(&self, p: &[u8], addr: SocketAddr) -> Result<usize, util::Error> {
        self.send_packet(p, addr, true).await
    }

    async fn send_control_to(&self, p: &[u8], addr: SocketAddr) -> Result<usize, util::Error> {
        self.send_packet(p, addr, false).await
    }

    async fn prepare_peer(&self, peer: SocketAddr) -> Result<(), util::Error> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(util::Error::Other("TURN allocation closed".into())),
            result = async {
                let mut state = self.relay_conn.lock().await;
                let permission = state.ensure_entry(peer).await.map_err(|error| util::Error::Other(error.to_string()))?;
                permission.add_connection();
                Ok(())
            } => result,
        }
    }

    async fn release_peer(&self, peer: SocketAddr) {
        let state = self.relay_conn.lock().await;
        let Some(permission) = state.perm_map.find(&peer).cloned() else {
            return;
        };
        let Some(retirement) = permission.remove_connection() else {
            return;
        };
        drop(state);
        let weak = Arc::downgrade(&self.relay_conn);
        let cancel = self.cancel.clone();
        self.request_tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = retirement.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(300)) => {},
            }
            let Some(state) = weak.upgrade() else {
                return;
            };
            let mut state = state.lock().await;
            if retirement.is_cancelled()
                || !permission.unused()
                || !state
                    .perm_map
                    .find(&peer)
                    .is_some_and(|entry| Arc::ptr_eq(entry, &permission))
            {
                return;
            }
            permission.cancel.cancel();
            permission.tasks.close();
            permission.tasks.wait().await;
            state.perm_map.remove(&peer);
            state.binding_mgr.lock().await.delete_by_addr(&peer);
            state
                .failed_peer_addrs
                .lock()
                .expect("TURN failed peers poisoned")
                .remove(&peer);
            log::debug!("TURN unused entry retired after 300s: {peer}");
        });
    }

    /// Returns the local network address.
    fn local_addr(&self) -> Result<SocketAddr, util::Error> {
        Ok(self.relayed_addr)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    fn take_failed_peer_addrs(&self) -> Vec<SocketAddr> {
        self.failed_peer_addrs
            .lock()
            .expect("TURN failed peer set mutex poisoned")
            .drain()
            .collect()
    }

    /// Closes the connection.
    /// Any blocked [`Self::recv_from()`] or [`Self::send_to()`] operations
    /// will be unblocked and return errors.
    async fn close(&self) -> Result<(), util::Error> {
        self.cancel.cancel();
        self.request_tasks.close();
        self.request_tasks.wait().await;

        let mut relay_conn = self.relay_conn.lock().await;
        let _ = relay_conn
            .close()
            .await
            .map_err(|err| util::Error::Other(format!("{err}")));
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

impl<T: RelayConnObserver + Send + Sync> RelayConnInternal<T> {
    /// Creates a new [`RelayConnInternal`].
    fn new(
        obs: Arc<Mutex<T>>,
        config: RelayConnConfig,
        allocation_failure_reason: Arc<StdMutex<Option<String>>>,
        failed_peer_addrs: Arc<StdMutex<HashSet<SocketAddr>>>,
        request_tasks: TaskTracker,
        cancel: CancellationToken,
    ) -> Self {
        RelayConnInternal {
            obs,
            relayed_addr: config.relayed_addr,
            perm_map: PermissionMap::new(),
            binding_mgr: config.binding_mgr,
            nonce: Arc::new(StdMutex::new(config.nonce)),
            allocation_failed: false,
            allocation_failure_reason,
            failed_peer_addrs,
            request_tasks,
            cancel,
        }
    }

    /// Writes a packet with payload `p` to `addr`.
    /// It can be made to time out and return
    /// an Error with Timeout() == true after a fixed time limit;
    /// see SetDeadline and SetWriteDeadline.
    /// On packet-oriented connections, write timeouts are rare.
    async fn send_to(&mut self, p: &[u8], addr: SocketAddr, payload: bool) -> Result<usize, Error> {
        if self.allocation_failed || self.cancel.is_cancelled() {
            return Err(Error::Other(
                "TURN allocation is no longer usable".to_owned(),
            ));
        }

        // Sending does not create permissions as a side effect.
        // Connection construction owns entry creation.
        if self.perm_map.find(&addr).is_none() {
            return Err(Error::Other(format!("TURN has no entry for {addr}")));
        }

        let (bind_state, bind_number) = {
            let binding_mgr = self.binding_mgr.lock().await;
            let binding = binding_mgr
                .find_by_addr(&addr)
                .ok_or(Error::ErrChannelBindNotFound)?;
            (binding.state(), binding.number)
        };

        let sent = match bind_state {
            BindingState::Ready => self.send_channel_data(p, bind_number).await,
            BindingState::Idle | BindingState::Request => {
                if bind_state == BindingState::Idle && payload {
                    {
                        let mut binding_mgr = self.binding_mgr.lock().await;
                        if let Some(binding) = binding_mgr.get_by_addr(&addr) {
                            binding.set_state(BindingState::Request);
                        }
                    }
                    self.spawn_channel_bind(addr, bind_number).await?;
                }
                self.send_indication(p, addr).await
            }
        }?;
        if sent == 0 {
            return Err(Error::Other("TURN transport sent no bytes".into()));
        }
        // The Port returns the original packet length, not TURN envelope bytes.
        Ok(p.len())
    }

    async fn ensure_entry(&mut self, addr: SocketAddr) -> Result<Arc<Permission>, Error> {
        let perm = if let Some(perm) = self.perm_map.find(&addr) {
            Arc::clone(perm)
        } else {
            let perm = Arc::new(Permission::new(self.cancel.child_token()));
            self.perm_map.insert(&addr, Arc::clone(&perm));
            perm
        };

        match perm.state() {
            PermState::Idle => {
                perm.set_state(PermState::Request);
                self.spawn_create_permission(Arc::clone(&perm), addr)
                    .await?;
            }
            PermState::Request | PermState::Permitted | PermState::Stopped => {}
        }

        {
            let mut binding_mgr = self.binding_mgr.lock().await;
            if binding_mgr.find_by_addr(&addr).is_none() {
                binding_mgr.create(addr).ok_or_else(|| {
                    Error::Other("TURN channel number space exhausted".to_owned())
                })?;
            }
        };
        Ok(perm)
    }

    async fn send_indication(&self, data: &[u8], addr: SocketAddr) -> Result<usize, Error> {
        let mut msg = Message::new();
        msg.build(&[
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_SEND, CLASS_INDICATION)),
            Box::new(proto::data::Data(data.to_vec())),
            Box::new(socket_addr2peer_address(&addr)),
            Box::new(FINGERPRINT),
        ])?;

        let obs = self.obs.lock().await;
        let turn_server_addr = obs.turn_server_addr();
        Ok(obs.write_to(&msg.raw, &turn_server_addr).await?)
    }

    async fn spawn_channel_bind(&self, addr: SocketAddr, channel_number: u16) -> Result<(), Error> {
        let permission = self
            .perm_map
            .find(&addr)
            .cloned()
            .ok_or(Error::ErrChannelBindNotFound)?;
        let transaction = Self::begin_entry_request(
            &self.obs,
            &self.nonce,
            addr,
            Some(channel_number),
            Duration::ZERO,
        )
        .await?;
        let (binding_mgr, obs, nonce, failed_peer_addrs, cancel) = (
            self.binding_mgr.clone(),
            self.obs.clone(),
            self.nonce.clone(),
            self.failed_peer_addrs.clone(),
            permission.cancel.clone(),
        );
        self.request_tasks.spawn(permission.tasks.track_future(async move {
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                result = async {
                    let mut transaction = transaction;
                    loop {
                        Self::complete_entry_request(&obs, &nonce, addr, Some(channel_number), transaction).await?;
                        {
                            let mut bindings = binding_mgr.lock().await;
                            let Some(binding) = bindings.get_by_addr(&addr) else { return Ok::<(),Error>(()); };
                            if binding.number != channel_number { return Ok(()); }
                            binding.set_state(BindingState::Ready);
                        }
                        // The next request is queued on success. The channel
                        // remains bound while that refresh is outstanding.
                        transaction = Self::begin_entry_request(&obs, &nonce, addr, Some(channel_number), TURN_ENTRY_REFRESH_INTERVAL).await?;
                    }
                } => result,
            };
            let mut binding_mgr = binding_mgr.lock().await;
            if let Some(binding) = binding_mgr.get_by_addr(&addr) {
                if cancel.is_cancelled() || binding.number != channel_number { return; }
                match result {
                    Ok(()) => {}
                    Err(error) => {
                        binding.set_state(BindingState::Idle);
                        failed_peer_addrs
                            .lock()
                            .expect("TURN failed peer set mutex poisoned")
                            .insert(addr);
                        log::warn!("TURN channel binding for {addr} failed: {error}");
                    }
                }
            }
        }));
        Ok(())
    }

    async fn spawn_create_permission(
        &self,
        permission: Arc<Permission>,
        addr: SocketAddr,
    ) -> Result<(), Error> {
        let transaction =
            Self::begin_entry_request(&self.obs, &self.nonce, addr, None, Duration::ZERO).await?;
        let binding_mgr = self.binding_mgr.clone();
        let (obs, nonce, failed_peer_addrs, cancel) = (
            self.obs.clone(),
            self.nonce.clone(),
            self.failed_peer_addrs.clone(),
            permission.cancel.clone(),
        );
        let tasks = permission.tasks.clone();
        self.request_tasks.spawn(tasks.track_future(async move {
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                result = async {
                    let mut transaction = transaction;
                    loop {
                        Self::complete_entry_request(&obs, &nonce, addr, None, transaction).await?;
                        permission.set_state(PermState::Permitted);
                        // Bound state is checked at response time, not when
                        // the already queued permission refresh fires later.
                        if binding_mgr.lock().await.find_by_addr(&addr).is_some_and(|binding| binding.state() == BindingState::Ready) {
                            return Ok::<(), Error>(());
                        }
                        transaction = Self::begin_entry_request(&obs, &nonce, addr, None, TURN_ENTRY_REFRESH_INTERVAL).await?;
                    }
                } => result,
            };
            match result {
                Ok(()) => {
                    permission.set_state(PermState::Permitted);
                }
                Err(error) => {
                    permission.set_state(PermState::Stopped);
                    failed_peer_addrs
                        .lock()
                        .expect("TURN failed peer set mutex poisoned")
                        .insert(addr);
                    log::warn!("TURN permission for {addr} failed: {error}");
                }
            }
        }));
        Ok(())
    }

    async fn send_channel_data(&self, data: &[u8], ch_num: u16) -> Result<usize, Error> {
        let mut ch_data = proto::chandata::ChannelData {
            data: data.to_vec(),
            number: proto::channum::ChannelNumber(ch_num),
            ..Default::default()
        };
        ch_data.encode();

        let obs = self.obs.lock().await;
        Ok(obs.write_to(&ch_data.raw, &obs.turn_server_addr()).await?)
    }

    async fn begin_entry_request(
        obs: &Arc<Mutex<T>>,
        nonce: &Arc<StdMutex<Nonce>>,
        addr: SocketAddr,
        channel_number: Option<u16>,
        delay: Duration,
    ) -> Result<Transaction, Error> {
        let mut transaction = {
            let obs = obs.lock().await;
            let msg = {
                let mut setters: Vec<Box<dyn Setter>> = vec![
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(
                        if channel_number.is_some() {
                            METHOD_CHANNEL_BIND
                        } else {
                            METHOD_CREATE_PERMISSION
                        },
                        CLASS_REQUEST,
                    )),
                    Box::new(socket_addr2peer_address(&addr)),
                ];
                if let Some(number) = channel_number {
                    setters.push(Box::new(proto::channum::ChannelNumber(number)));
                }
                setters.extend(obs.auth_setters(&nonce.lock().expect("TURN nonce mutex poisoned")));
                setters.push(Box::new(FINGERPRINT));

                let mut msg = Message::new();
                msg.build(&setters)?;
                msg
            };

            let turn_server_addr = obs.turn_server_addr();

            log::debug!("TURN entry request {} for {addr}", msg.typ);
            obs.transaction_after(&msg, &turn_server_addr, false, delay)?
        };
        if delay.is_zero() {
            transaction.wait_first_send().await?;
        }
        Ok(transaction)
    }

    async fn complete_entry_request(
        obs: &Arc<Mutex<T>>,
        nonce: &Arc<StdMutex<Nonce>>,
        addr: SocketAddr,
        channel_number: Option<u16>,
        mut transaction: Transaction,
    ) -> Result<(), Error> {
        let method = if channel_number.is_some() {
            METHOD_CHANNEL_BIND
        } else {
            METHOD_CREATE_PERMISSION
        };
        loop {
            let response = transaction.wait().await?.msg;
            if response.typ == MessageType::new(method, CLASS_SUCCESS_RESPONSE) {
                return Ok(());
            }
            if response.typ.class == CLASS_ERROR_RESPONSE {
                let mut code = ErrorCodeAttribute::default();
                code.get_from(&response)?;
                if code.code == CODE_STALE_NONCE {
                    if Self::update_shared_auth(obs, nonce, &response).await {
                        transaction = Self::begin_entry_request(
                            obs,
                            nonce,
                            addr,
                            channel_number,
                            Duration::ZERO,
                        )
                        .await?;
                        continue;
                    }
                    // The entry is left without a replacement request when
                    // a 438 lacks usable auth. Entry destruction cancels it.
                    return std::future::pending().await;
                }
                return Err(Error::Other(format!(
                    "TURN {} for {addr}: {code}",
                    response.typ
                )));
            }
            return Err(Error::ErrUnexpectedResponse);
        }
    }

    async fn update_shared_auth(
        obs: &Arc<Mutex<T>>,
        nonce_slot: &Arc<StdMutex<Nonce>>,
        msg: &Message,
    ) -> bool {
        let Ok(realm) = Realm::get_from_as(msg, ATTR_REALM) else {
            log::warn!("TURN stale-nonce response omitted realm");
            return false;
        };
        let mut owner = obs.lock().await;
        owner.set_realm(realm);
        let Ok(nonce) = Nonce::get_from_as(msg, ATTR_NONCE) else {
            log::warn!("TURN stale-nonce response omitted nonce");
            return false;
        };

        *nonce_slot.lock().expect("TURN nonce mutex poisoned") = nonce;
        true
    }

    /// Closes the connection.
    /// Any blocked `recv_from` or `send_to` operations will be unblocked and return errors.
    pub async fn close(&mut self) -> Result<(), Error> {
        Self::begin_allocation_refresh(
            &self.obs,
            &self.nonce,
            Some(Duration::ZERO),
            true,
            Duration::ZERO,
        )
        .await?
        .wait()
        .await
        .map(|_| ())
    }

    async fn begin_allocation_refresh(
        obs: &Arc<Mutex<T>>,
        nonce: &Arc<StdMutex<Nonce>>,
        lifetime: Option<Duration>,
        dont_wait: bool,
        delay: Duration,
    ) -> Result<Transaction, Error> {
        let transaction = {
            let obs = obs.lock().await;

            let msg = {
                let mut setters: Vec<Box<dyn Setter>> = vec![
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)),
                ];
                if let Some(lifetime) = lifetime {
                    setters.push(Box::new(proto::lifetime::Lifetime(lifetime)));
                }
                setters.extend(obs.auth_setters(&nonce.lock().expect("TURN nonce mutex poisoned")));
                setters.push(Box::new(FINGERPRINT));
                let mut msg = Message::new();
                msg.build(&setters)?;
                msg
            };

            log::debug!("send refresh request (dont_wait={dont_wait})");
            let turn_server_addr = obs.turn_server_addr();
            obs.transaction_after(&msg, &turn_server_addr, dont_wait, delay)?
        };
        Ok(transaction)
    }

    async fn allocation_refresh_chain(
        obs: &Arc<Mutex<T>>,
        nonce: &Arc<StdMutex<Nonce>>,
        mut transaction: Transaction,
    ) -> Result<(), Error> {
        loop {
            match Self::complete_allocation_refresh(obs, nonce, transaction).await {
                Ok(lifetime) if lifetime.is_zero() => {
                    return Err(Error::Other(
                        "TURN server ended the allocation (lifetime zero)".into(),
                    ))
                }
                Ok(lifetime) => {
                    transaction = Self::begin_allocation_refresh(
                        obs,
                        nonce,
                        None,
                        false,
                        allocation_refresh_delay(lifetime),
                    )
                    .await?
                }
                Err(Error::ErrTryAgain) => {
                    transaction =
                        Self::begin_allocation_refresh(obs, nonce, None, false, Duration::ZERO)
                            .await?
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn complete_allocation_refresh(
        obs: &Arc<Mutex<T>>,
        nonce: &Arc<StdMutex<Nonce>>,
        transaction: Transaction,
    ) -> Result<Duration, Error> {
        let res = transaction.wait().await?.msg;

        if res.typ.class == CLASS_ERROR_RESPONSE {
            let mut code = ErrorCodeAttribute::default();
            let result = code.get_from(&res);
            if result.is_err() {
                return Err(Error::Other(format!("{}", res.typ)));
            } else if code.code == CODE_STALE_NONCE {
                if Self::update_shared_auth(obs, nonce, &res).await {
                    return Err(Error::ErrTryAgain);
                }
                return std::future::pending().await;
            } else {
                return Err(Error::Other(format!("{} (error {})", res.typ, code)));
            }
        }

        if res.typ != MessageType::new(METHOD_REFRESH, CLASS_SUCCESS_RESPONSE) {
            return Err(Error::ErrUnexpectedResponse);
        }

        // Getting lifetime from response
        let mut updated_lifetime = proto::lifetime::Lifetime::default();
        if updated_lifetime.get_from(&res).is_err() {
            // No lifetime means no refresh callback/reschedule, not a
            // fabricated allocation error. The enclosing timer remains cancellable.
            log::warn!("TURN refresh success omitted lifetime; awaiting owner cancellation");
            return std::future::pending().await;
        }

        log::debug!("updated lifetime: {} seconds", updated_lifetime.0.as_secs());
        Ok(updated_lifetime.0)
    }
}

fn socket_addr2peer_address(addr: &SocketAddr) -> proto::peeraddr::PeerAddress {
    proto::peeraddr::PeerAddress {
        ip: addr.ip(),
        port: addr.port(),
    }
}
