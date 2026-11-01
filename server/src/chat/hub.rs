//! Chat connections (`docs/protocol/chat-design.md` §6.1): one per device,
//! apart from its presence connection; who is online, idle, typing.

use crate::hub::Outbound;
use farwing_protocol::chat::Status;
use farwing_protocol::realtime::{Envelope, close};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Typing notices a user sends per channel at most this often.
const TYPING_EVERY: Duration = Duration::from_secs(3);

struct Conn {
    conn_id: String,
    user_id: String,
    auth: crate::hub::LinkAuth,
    tx: UnboundedSender<Outbound>,
    idle: bool,
    /// It said `chat.hello`: pushes go to it.
    ready: bool,
}

#[derive(Default)]
struct State {
    /// By device.
    conns: HashMap<String, Conn>,
    /// The status each user was last told to be in.
    told: HashMap<String, Status>,
    typing: HashMap<(String, i64), Instant>,
}

#[derive(Default)]
pub struct ChatHub {
    st: Mutex<State>,
}

impl ChatHub {
    /// A device's new chat connection; it replaces its previous one (4001).
    pub fn attach(
        &self,
        device: &str,
        auth: &crate::hub::LinkAuth,
        conn_id: &str,
        tx: UnboundedSender<Outbound>,
    ) {
        let mut st = self.st.lock();
        if let Some(old) = st.conns.insert(
            device.to_owned(),
            Conn {
                conn_id: conn_id.to_owned(),
                user_id: auth.user_id().unwrap_or_default().to_owned(),
                auth: auth.clone(),
                tx,
                idle: false,
                ready: false,
            },
        ) {
            let _ = old
                .tx
                .send(Outbound::Close(close::SUPERSEDED, "superseded by a newer connection".into()));
        }
    }

    /// The connection ended; whose it was, when it was still the device's.
    pub fn detach(&self, device: &str, conn_id: &str) -> Option<String> {
        let mut st = self.st.lock();
        if st.conns.get(device).is_some_and(|c| c.conn_id == conn_id) {
            st.conns.remove(device).map(|c| c.user_id)
        } else {
            None
        }
    }

    pub fn set_ready(&self, device: &str, conn_id: &str, idle: bool) {
        if let Some(c) = self.st.lock().conns.get_mut(device).filter(|c| c.conn_id == conn_id) {
            c.ready = true;
            c.idle = idle;
        }
    }

    pub fn set_idle(&self, device: &str, conn_id: &str, idle: bool) {
        if let Some(c) = self.st.lock().conns.get_mut(device).filter(|c| c.conn_id == conn_id) {
            c.idle = idle;
        }
    }

    /// Send `env` to every ready connection of `user` but `except` (a device).
    pub fn send_user(&self, user: &str, except: Option<&str>, env: &Envelope) {
        let text = env.encode();
        let st = self.st.lock();
        for (device, c) in &st.conns {
            if c.ready && c.user_id == user && Some(device.as_str()) != except {
                let _ = c.tx.send(Outbound::Text(text.clone()));
            }
        }
    }

    /// Send `env` to `device`'s connection, when ready.
    pub fn send_device(&self, device: &str, env: &Envelope) {
        if let Some(c) = self.st.lock().conns.get(device).filter(|c| c.ready) {
            let _ = c.tx.send(Outbound::Text(env.encode()));
        }
    }

    /// Send `env` to every ready connection of the users in `users`.
    pub fn send_users<'a>(&self, users: impl IntoIterator<Item = &'a String>, env: &Envelope) {
        let users: HashSet<&String> = users.into_iter().collect();
        let text = env.encode();
        let st = self.st.lock();
        for c in st.conns.values() {
            if c.ready && users.contains(&c.user_id) {
                let _ = c.tx.send(Outbound::Text(text.clone()));
            }
        }
    }

    /// Users with a chat connection.
    pub fn online(&self) -> HashSet<String> {
        self.st.lock().conns.values().map(|c| c.user_id.clone()).collect()
    }

    /// `None`: no connection; `Some(true)`: every connection idle.
    pub fn activity(&self, user: &str) -> Option<bool> {
        let st = self.st.lock();
        let mut any = false;
        let mut all_idle = true;
        for c in st.conns.values().filter(|c| c.user_id == user) {
            any = true;
            all_idle &= c.idle;
        }
        any.then_some(all_idle)
    }

    /// Remember `status` as what others were told; whether it changed.
    pub fn tell(&self, user: &str, status: Status) -> bool {
        let mut st = self.st.lock();
        let before = st.told.insert(user.to_owned(), status);
        if status == Status::Offline {
            st.told.remove(user);
        }
        before.unwrap_or(Status::Offline) != status
    }

    /// Whether `user` may tell others it types in `channel` now.
    pub fn typing(&self, user: &str, channel: i64) -> bool {
        let mut st = self.st.lock();
        let now = Instant::now();
        st.typing.retain(|_, t| now.duration_since(*t) < TYPING_EVERY);
        let key = (user.to_owned(), channel);
        if st.typing.contains_key(&key) {
            return false;
        }
        st.typing.insert(key, now);
        true
    }

    /// Close the connections whose device and credential match (a
    /// revocation).
    pub fn revoke_where(&self, matches: &dyn Fn(&str, &crate::hub::LinkAuth) -> bool) {
        let st = self.st.lock();
        for (device, c) in &st.conns {
            if matches(device, &c.auth) {
                let _ = c.tx.send(Outbound::Close(close::REVOKED, "credentials revoked".into()));
            }
        }
    }

    /// Close the connections of `user` (its account went away).
    pub fn close_user(&self, user: &str) {
        let st = self.st.lock();
        for c in st.conns.values().filter(|c| c.user_id == user) {
            let _ = c.tx.send(Outbound::Close(close::REVOKED, "account revoked".into()));
        }
    }

    /// Close the connection of `device` (signed out or unbound).
    pub fn close_device(&self, device: &str) {
        if let Some(c) = self.st.lock().conns.get(device) {
            let _ = c.tx.send(Outbound::Close(close::REVOKED, "device revoked".into()));
        }
    }

    pub fn shutdown(&self) {
        let st = self.st.lock();
        for c in st.conns.values() {
            let _ = c.tx.send(Outbound::Close(1001, "server shutting down".into()));
        }
    }

    pub fn count(&self) -> usize {
        self.st.lock().conns.len()
    }
}
