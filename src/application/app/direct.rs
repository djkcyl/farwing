//! State of the "设备" and "本机" pages (`docs/access-model.md` §3): the one
//! connect bar, the device list's tabs and LAN discovery, and changes to
//! the devices this one trusts.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The device list's tabs: one list of devices, filtered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum DeviceTab {
    /// This account's computers.
    #[default]
    Mine,
    /// Devices friends share with this account.
    Friends,
    /// Devices answering on the local network.
    Nearby,
    /// Devices this one connected to, and saved favourites.
    Recent,
}

#[derive(Default)]
pub(super) struct LanState {
    pub scanning: bool,
    pub found: Option<Vec<crate::transport::direct::lan::Found>>,
    pub error: Option<String>,
    pub scanned_at: Option<Instant>,
    /// When each device last answered: one that misses a round stays listed
    /// for a while, so the list does not flicker.
    seen: std::collections::HashMap<String, Instant>,
}

/// While the "附近" tab is open, query again this long after each round.
const SCAN_GAP: Duration = Duration::from_secs(2);
/// After a failed query, wait longer.
const SCAN_RETRY: Duration = Duration::from_secs(10);
/// How long each query waits for answers.
const SCAN_WINDOW: Duration = Duration::from_millis(1500);
/// A device that stopped answering leaves the list after this long.
const SCAN_KEEP: Duration = Duration::from_secs(8);

/// Pairing with a nearby device: both screens show the same code, and
/// both users agree.
pub(super) struct Pairing {
    pub name: String,
    /// Known once the connection is up.
    pub code: Option<String>,
    /// This side's user agrees (taken when they do).
    pub agree: Option<tokio::sync::oneshot::Sender<()>>,
    /// The other device, once paired; or why not.
    pub outcome: Option<Result<farwing_protocol::direct::DirectHello, String>>,
    pub cancel: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
pub(super) struct DirectUi {
    pub tab: DeviceTab,
    /// The tab was chosen (by the user or for them); otherwise it follows
    /// whether this device is signed in.
    pub tab_chosen: bool,
    /// The connect bar: a device ID, a short number or an address.
    pub target: String,
    /// The connect bar's optional access code.
    pub code: String,
    /// Ask the other side's user to confirm instead of using a code.
    pub ask: bool,
    /// Move the keyboard focus to the code field once.
    pub focus_code: bool,
    pub lan: Arc<Mutex<LanState>>,
    pub message: Option<(String, Instant)>,
    pub error: Option<String>,
    /// Editing what a trusted device may do: device ID, name and
    /// capabilities.
    pub editing: Option<(String, String, Vec<String>)>,
    /// A trusted device to stop trusting once confirmed: ID and name.
    pub confirm_remove: Option<(String, String)>,
    /// This machine's LAN addresses, read at most every few seconds.
    addresses: Option<(Instant, Vec<std::net::Ipv4Addr>)>,
    /// A change sent to the background service is in progress.
    pub sending: bool,
    /// Pairing in progress (or just finished, until picked up).
    pub pairing: Arc<Mutex<Option<Pairing>>>,
    outcome: Arc<Mutex<Option<Result<String, String>>>>,
}

impl DirectUi {
    pub fn lan(&self) -> std::sync::MutexGuard<'_, LanState> {
        self.lan.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Query the LAN in the background; the page repaints with the result.
    pub fn scan(&self, ctx: &egui::Context) {
        {
            let mut lan = self.lan();
            if lan.scanning {
                return;
            }
            lan.scanning = true;
            lan.error = None;
        }
        let state = Arc::clone(&self.lan);
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("lan-discovery".into())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|runtime| {
                        runtime.block_on(crate::transport::direct::lan::discover(SCAN_WINDOW))
                    });
                {
                    let mut lan = state.lock().unwrap_or_else(|e| e.into_inner());
                    lan.scanning = false;
                    let now = Instant::now();
                    lan.scanned_at = Some(now);
                    match result {
                        Ok(found) => {
                            for host in &found {
                                lan.seen.insert(host.announce.device_id.clone(), now);
                            }
                            let mut merged = found;
                            for host in lan.found.take().unwrap_or_default() {
                                let recent = lan
                                    .seen
                                    .get(&host.announce.device_id)
                                    .is_some_and(|at| now.duration_since(*at) < SCAN_KEEP);
                                if recent
                                    && !merged
                                        .iter()
                                        .any(|h| h.announce.device_id == host.announce.device_id)
                                {
                                    merged.push(host);
                                }
                            }
                            // Stable between scans, also for devices of the same name.
                            merged.sort_by(|a, b| {
                                crate::account::api::list_order(
                                    &a.announce.name,
                                    &a.announce.device_id,
                                    &b.announce.name,
                                    &b.announce.device_id,
                                )
                            });
                            lan.seen.retain(|_, at| now.duration_since(*at) < SCAN_KEEP);
                            lan.found = Some(merged);
                            lan.error = None;
                        }
                        Err(error) => lan.error = Some(format!("局域网发现失败：{error:#}")),
                    }
                }
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.lan().scanning = false;
        }
    }

    /// Pair with the device `device_id` at `target`, in the background.
    pub fn pair(
        &self,
        ctx: &egui::Context,
        target: crate::transport::direct::Target,
        device_id: String,
        name: String,
    ) {
        let cancel = tokio_util::sync::CancellationToken::new();
        let (agree, agreed) = tokio::sync::oneshot::channel();
        {
            let mut slot = self.pairing.lock().unwrap_or_else(|e| e.into_inner());
            if slot.as_ref().is_some_and(|p| p.outcome.is_none()) {
                return;
            }
            *slot = Some(Pairing {
                name,
                code: None,
                agree: Some(agree),
                outcome: None,
                cancel: cancel.clone(),
            });
        }
        let slot = Arc::clone(&self.pairing);
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("direct-pairing".into())
            .spawn(move || {
                let shown = {
                    let slot = Arc::clone(&slot);
                    let ctx = ctx.clone();
                    move |code: String| {
                        if let Some(pairing) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                            pairing.code = Some(code);
                        }
                        ctx.request_repaint();
                    }
                };
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|runtime| {
                        runtime.block_on(async {
                            let local = crate::transport::direct::local_device()?;
                            crate::transport::direct::client::pair(
                                &target, &device_id, &local, shown, agreed, &cancel,
                            )
                            .await
                        })
                    })
                    .map_err(|error| match error.downcast_ref::<crate::account::api::ApiFailure>() {
                        // The host's own words, without the protocol code.
                        Some(failure) if !failure.message.is_empty() => failure.message.clone(),
                        _ => format!("{error:#}"),
                    });
                if let Some(pairing) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    pairing.outcome = Some(result);
                }
                ctx.request_repaint();
            });
        if spawned.is_err() {
            *self.pairing.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// While the "附近" tab is shown: query the LAN again shortly after
    /// each round, quietly (the list changes only when the answers do).
    pub fn scan_continuously(&self, ctx: &egui::Context) {
        let wait = {
            let lan = self.lan();
            if lan.scanning {
                return;
            }
            let gap = if lan.error.is_some() { SCAN_RETRY } else { SCAN_GAP };
            lan.scanned_at
                .map_or(Duration::ZERO, |at| gap.saturating_sub(at.elapsed()))
        };
        if wait.is_zero() {
            self.scan(ctx);
        } else {
            ctx.request_repaint_after(wait);
        }
    }

    /// This side's user agrees to the pairing shown.
    pub fn agree_pairing(&self) {
        if let Some(agree) = self
            .pairing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .and_then(|p| p.agree.take())
        {
            let _ = agree.send(());
        }
    }

    /// A pairing waits for the other side.
    pub fn pairing_active(&self) -> bool {
        self.pairing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|p| p.outcome.is_none())
    }

    /// The devices found on the LAN, without this one.
    pub fn nearby(&self) -> Vec<crate::transport::direct::lan::Found> {
        let local = crate::transport::direct::local_device_id().unwrap_or_default();
        self.lan()
            .found
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|host| host.announce.device_id != local)
            .collect()
    }

    /// This machine's LAN addresses, for telling a peer where to connect.
    pub fn local_addresses(&mut self) -> Vec<std::net::Ipv4Addr> {
        if self
            .addresses
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(10))
        {
            self.addresses = Some((
                Instant::now(),
                crate::platform::windows::device_profile::lan_ipv4_addresses(),
            ));
        }
        self.addresses
            .as_ref()
            .map(|(_, list)| list.clone())
            .unwrap_or_default()
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now() + Duration::from_secs(4)));
        self.error = None;
    }

    /// Change the host side: ask the background service (which hosts in
    /// service mode) or apply it in this process. The result of a service
    /// request arrives through [`Self::take_outcome`].
    pub fn perform(
        &mut self,
        ctx: &egui::Context,
        host: Option<&crate::features::host::Handle>,
        action: crate::transport::direct::managed::Action,
        done: impl Into<String>,
    ) {
        if crate::transport::direct::managed::managed() {
            self.send(ctx, host, action, done);
            return;
        }
        match crate::transport::direct::managed::perform(host, action) {
            Ok(()) => self.note(done),
            Err(error) => self.error = Some(format!("{error:#}")),
        }
    }

    /// Ask the background service (which hosts in service mode) to apply
    /// `action`; the result arrives through [`Self::take_outcome`].
    pub fn send(
        &mut self,
        ctx: &egui::Context,
        host: Option<&crate::features::host::Handle>,
        action: crate::transport::direct::managed::Action,
        done: impl Into<String>,
    ) {
        let Some(account) = host.map(|host| host.account_generation()) else {
            self.error = Some("后台服务尚未就绪，请稍后再试".into());
            return;
        };
        let done = done.into();
        let slot = Arc::clone(&self.outcome);
        let ctx = ctx.clone();
        self.sending = true;
        let spawned = std::thread::Builder::new()
            .name("direct-service".into())
            .spawn(move || {
                let result = crate::transport::direct::managed::request(account, action)
                    .map(|()| done)
                    .map_err(|error| format!("{error:#}"));
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.sending = false;
            self.error = Some("无法联系后台服务".into());
        }
    }

    /// The result of the last [`Self::send`], once.
    pub fn take_outcome(&mut self) -> Option<Result<String, String>> {
        let outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner()).take();
        if outcome.is_some() {
            self.sending = false;
        }
        outcome
    }

    /// Expire the shown message, and pick up a service result.
    pub fn poll_messages(&mut self) {
        if self
            .message
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            self.message = None;
        }
        match self.take_outcome() {
            Some(Ok(message)) => self.note(message),
            Some(Err(error)) => self.error = Some(error),
            None => {}
        }
    }
}
