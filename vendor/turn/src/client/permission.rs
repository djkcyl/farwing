use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use portable_atomic::AtomicU8;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Copy, Clone, PartialEq, Debug)]
pub(crate) enum PermState {
    Idle = 0,
    Request = 1,
    Permitted = 2,
    Stopped = 3,
}

impl From<u8> for PermState {
    fn from(v: u8) -> Self {
        match v {
            0 => PermState::Idle,
            1 => PermState::Request,
            2 => PermState::Permitted,
            _ => PermState::Stopped,
        }
    }
}

pub(crate) struct Permission {
    st: AtomicU8,
    pub(crate) cancel: CancellationToken,
    pub(crate) tasks: TaskTracker,
    connections: Mutex<usize>,
    retirement: Mutex<Option<CancellationToken>>,
}

impl Default for Permission {
    fn default() -> Self {
        Self {
            st: AtomicU8::new(PermState::Idle as u8),
            cancel: CancellationToken::new(),
            tasks: TaskTracker::new(),
            connections: Mutex::new(0),
            retirement: Mutex::new(None),
        }
    }
}

impl Permission {
    pub(crate) fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            ..Self::default()
        }
    }
    pub(crate) fn add_connection(&self) {
        let mut connections = self.connections.lock().expect("TURN entry refs poisoned");
        if *connections == 0 {
            if let Some(retirement) = self
                .retirement
                .lock()
                .expect("TURN entry retirement poisoned")
                .take()
            {
                retirement.cancel();
            }
        }
        *connections += 1;
    }
    pub(crate) fn remove_connection(&self) -> Option<CancellationToken> {
        let mut connections = self.connections.lock().expect("TURN entry refs poisoned");
        if *connections == 0 {
            return None;
        }
        *connections -= 1;
        if *connections != 0 {
            return None;
        }
        let retirement = self.cancel.child_token();
        *self
            .retirement
            .lock()
            .expect("TURN entry retirement poisoned") = Some(retirement.clone());
        Some(retirement)
    }
    pub(crate) fn unused(&self) -> bool {
        *self.connections.lock().expect("TURN entry refs poisoned") == 0
    }
    pub(crate) fn set_state(&self, state: PermState) {
        self.st.store(state as u8, Ordering::SeqCst);
    }

    pub(crate) fn state(&self) -> PermState {
        self.st.load(Ordering::SeqCst).into()
    }
}

/// Thread-safe Permission map.
#[derive(Default)]
pub(crate) struct PermissionMap {
    perm_map: HashMap<SocketAddr, Arc<Permission>>,
}

impl PermissionMap {
    pub(crate) fn new() -> PermissionMap {
        PermissionMap {
            perm_map: HashMap::new(),
        }
    }

    pub(crate) fn insert(&mut self, addr: &SocketAddr, p: Arc<Permission>) {
        self.perm_map.insert(*addr, p);
    }

    pub(crate) fn find(&self, addr: &SocketAddr) -> Option<&Arc<Permission>> {
        self.perm_map.get(addr)
    }

    pub(crate) fn remove(&mut self, addr: &SocketAddr) -> Option<Arc<Permission>> {
        self.perm_map.remove(addr)
    }
}
