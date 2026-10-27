//! Direct connections without the server: the host's direct endpoint, its
//! handshake messages and LAN discovery (`docs/protocol/v2-design.md` §12).
//!
//! After the handshake both sides exchange the same [`crate::realtime::Envelope`]
//! messages as on the realtime plane (`session.start`, `rtc.*`,
//! `session.end`), framed as a big-endian `u32` length followed by the JSON
//! text.

use crate::Platform;
use crate::api::Relation;
use serde::{Deserialize, Serialize};

/// Default TCP port of the direct endpoint. The UDP ports of direct sessions
/// start at the same number (see [`UDP_PORTS`]).
pub const DEFAULT_PORT: u16 = 28890;
/// Further TCP ports tried when [`DEFAULT_PORT`] (or the configured port) is
/// taken, e.g. by another profile on the same machine.
pub const PORT_FALLBACKS: u16 = 9;
/// UDP ports a host uses for the ICE of direct sessions, from the TCP port's
/// number: all direct sessions share this one UDP port. Forwarding it (and
/// the TCP port) on a router makes a host reachable from the internet.
pub const UDP_PORTS: u16 = 1;
/// LAN discovery: IPv4 multicast group and UDP port (below the default
/// direct port, outside the UDP range of direct sessions).
pub const DISCOVERY_GROUP: [u8; 4] = [239, 255, 70, 87];
pub const DISCOVERY_PORT: u16 = 28889;
/// Largest discovery datagram.
pub const MAX_DISCOVERY_BYTES: usize = 1200;
/// Largest direct frame.
pub const MAX_FRAME: usize = 1024 * 1024;
/// TLS exporter label of the keying material the handshake proofs bind to.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-farwing-direct-v2";
/// Length of the exported keying material.
pub const EXPORTER_LEN: usize = 32;
/// Handshake deadline of an unauthenticated connection.
pub const HANDSHAKE_TIMEOUT_MS: u64 = 10_000;
/// Keep-alive interval and idle limit of an established connection.
pub const PING_INTERVAL_MS: u64 = 15_000;
pub const IDLE_TIMEOUT_MS: u64 = 45_000;
/// Concurrent connections that have not finished their handshake.
pub const MAX_PENDING: usize = 16;

/// Handshake message types.
pub mod types {
    /// Both directions, first message: device identity and proof.
    pub const HELLO: &str = "direct.hello";
    /// Controller → host: authorise by device key (own device, friend or
    /// trusted device).
    pub const AUTH: &str = "direct.auth";
    /// Controller → host and host → controller: SPAKE2 messages.
    pub const PAKE: &str = "direct.pake";
    /// Controller → host: key confirmation after a PAKE.
    pub const CONFIRM: &str = "direct.confirm";
    /// Host → controller: authorised, with relation and capabilities.
    pub const AUTHORIZED: &str = "direct.authorized";
    /// Controller → host: pair (trust each other), committing to the
    /// controller's nonce ([`super::DirectPair`]). The host answers with
    /// its nonce, the controller reveals its own, and both users compare
    /// the code ([`crate::crypto::pairing_code`]) and agree.
    pub const PAIR: &str = "direct.pair";
    /// Host → controller: the host's nonce ([`super::DirectPairNonce`]).
    pub const PAIR_NONCE: &str = "direct.pair.nonce";
    /// Controller → host: the controller's nonce, as committed.
    pub const PAIR_REVEAL: &str = "direct.pair.reveal";
    /// Controller → host: the controller's user agrees.
    pub const PAIR_CONFIRM: &str = "direct.pair.confirm";
    /// Host → controller: both users agreed; each trusts the other.
    pub const PAIRED: &str = "direct.paired";
}

/// Authorisation methods a host accepts (`docs/access-model.md` §2.2).
pub mod method {
    /// A device of the host's account (cached device list).
    pub const OWN: &str = "own";
    /// A friend's device under the friend's grants (cached from the server).
    pub const FRIEND: &str = "friend";
    /// A device the host trusts.
    pub const TRUSTED: &str = "trusted";
    /// A visitor with the access code or the host user's confirmation.
    pub const ASSIST: &str = "assist";
}

/// PAKE purpose in [`DirectPake::kind`].
pub mod pake_kind {
    /// The access code.
    pub const ASSIST: &str = "assist";
}

/// `direct.hello`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectHello {
    pub device_id: String,
    /// base64url Ed25519 public key.
    pub public_key: String,
    pub name: String,
    pub platform: Platform,
    pub version: String,
    /// Signature over [`crate::crypto::direct_proof_message`].
    pub proof: String,
    /// Host only: methods accepted right now ([`method`]).
    pub methods: Vec<String>,
    /// Host only: the first UDP port of direct sessions.
    pub udp_port: u16,
    /// Host only: it admits this controller by its device key right now
    /// (one of its account's devices, a friend's or a trusted device), so
    /// neither an access code nor a confirmation is needed.
    pub admits: bool,
}

/// `direct.auth` (controller → host).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectAuth {
    /// [`method::OWN`], [`method::FRIEND`] or [`method::TRUSTED`]; the host
    /// checks all of them.
    pub method: String,
}

/// `direct.pake`. From the controller: `kind` and one message per slot
/// ([`crate::crypto::ASSIST_PAKE_SLOTS`], or none for a host that only asks
/// for confirmation). From the host: its messages and one key confirmation
/// per slot.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectPake {
    pub kind: String,
    pub messages: Vec<String>,
    pub confirm: Vec<String>,
    /// Host reply: whether the host still asks its user to confirm.
    pub confirmation: bool,
}

impl std::fmt::Debug for DirectPake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectPake")
            .field("kind", &self.kind)
            .field("messages", &self.messages.len())
            .finish_non_exhaustive()
    }
}

/// `direct.confirm` (controller → host).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectConfirm {
    pub confirm: String,
}

/// `direct.authorized` (host → controller): also what the host's
/// `direct.hello` leaves out before access (its name, system, version).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectAuthorized {
    pub relation: Relation,
    pub grants: Vec<String>,
    pub name: String,
    pub platform: crate::Platform,
    pub version: String,
}

/// `direct.pair` (controller → host).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectPair {
    /// Base64 of [`crate::crypto::pairing_commit`] of the controller's nonce.
    pub commit: String,
}

/// `direct.pair.nonce` (host → controller) and `direct.pair.reveal`
/// (controller → host).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectPairNonce {
    /// 32 random bytes, base64.
    pub nonce: String,
}

/// `direct.paired` (host → controller).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectPaired {
    /// What the controller may do on the host from now on.
    pub grants: Vec<String>,
    pub name: String,
    pub platform: crate::Platform,
    pub version: String,
}

/// LAN discovery messages ([`DISCOVERY_PORT`]).
pub mod lan {
    use serde::{Deserialize, Serialize};

    pub const QUERY: &str = "farwing.lan.query";
    pub const ANNOUNCE: &str = "farwing.lan.announce";

    /// `{"t":"farwing.lan.query","v":2,"nonce":"…"}`.
    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct Query {
        pub t: String,
        pub v: u32,
        pub nonce: String,
    }

    /// A host's announcement. `sig` signs the canonical JSON of every other
    /// field ([`Announce::signed_bytes`]).
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct Announce {
        pub t: String,
        pub v: u32,
        pub device_id: String,
        pub name: String,
        pub platform: crate::Platform,
        pub version: String,
        pub public_key: String,
        pub port: u16,
        pub udp_port: u16,
        pub methods: Vec<String>,
        /// Unix seconds.
        pub ts: i64,
        /// The query this answers; empty for a periodic announcement.
        pub nonce: String,
        pub sig: String,
    }

    impl Announce {
        /// Canonical bytes covered by `sig`: the JSON object without `sig`,
        /// keys sorted (serde_json maps are ordered by key).
        pub fn signed_bytes(&self) -> Vec<u8> {
            let mut value = serde_json::to_value(self).unwrap_or_default();
            if let Some(object) = value.as_object_mut() {
                object.remove("sig");
                let sorted: std::collections::BTreeMap<_, _> =
                    std::mem::take(object).into_iter().collect();
                let mut prefix = b"FWLAN2\n".to_vec();
                prefix.extend(serde_json::to_vec(&sorted).unwrap_or_default());
                return prefix;
            }
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_signature_bytes_ignore_sig_and_order() {
        let mut announce = lan::Announce {
            t: lan::ANNOUNCE.into(),
            v: 2,
            device_id: "a1b2c3d4e5f6g7h8".into(),
            name: "PC".into(),
            port: DEFAULT_PORT,
            ..Default::default()
        };
        let before = announce.signed_bytes();
        announce.sig = "x".into();
        assert_eq!(before, announce.signed_bytes());
        let text = String::from_utf8(before).unwrap();
        assert!(text.starts_with("FWLAN2\n{\"device_id\""));
        assert!(!text.contains("\"sig\""));
    }
}
