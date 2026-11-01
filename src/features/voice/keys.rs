//! Frame keys between devices (`docs/protocol/chat-design.md` §10.2).
//!
//! A participant's frame key goes to each other device sealed to that
//! device's X25519 key: a one-time X25519 key, HKDF-SHA256 (salt
//! `FWVOICE1`, info = one-time public ‖ recipient public) to an AES-256-GCM
//! key and nonce, the scope (channel or call) as associated data; the box
//! is signed by the sender's Ed25519 device key.

use crate::account::api::ClientIdentity;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Result, anyhow, ensure};
use farwing_protocol::chat::{DeviceX25519, VoiceKeyBox, VoiceKeyPush, key_box_message, x25519_message};
use farwing_protocol::crypto::{b64, unb64, verify};
use x25519_dalek::{PublicKey, StaticSecret};

pub(crate) const FRAME_KEY: usize = crate::media::frame_crypto::KEY_BYTES;

pub(crate) struct DeviceKeys {
    identity: ClientIdentity,
    secret: StaticSecret,
    public: String,
}

fn derive(shared: &[u8; 32], ephemeral: &[u8; 32], recipient: &[u8; 32]) -> ([u8; 32], [u8; 12]) {
    let mut info = Vec::with_capacity(64);
    info.extend_from_slice(ephemeral);
    info.extend_from_slice(recipient);
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(b"FWVOICE1"), shared);
    let mut okm = [0u8; 44];
    hk.expand(&info, &mut okm).expect("44 bytes is a valid length");
    let mut key = [0u8; 32];
    let mut nonce = [0u8; 12];
    key.copy_from_slice(&okm[..32]);
    nonce.copy_from_slice(&okm[32..]);
    (key, nonce)
}

fn key32(text: &str) -> Result<[u8; 32]> {
    unb64(text)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or_else(|| anyhow!("invalid X25519 key"))
}

impl DeviceKeys {
    pub(crate) fn new(identity: ClientIdentity) -> Self {
        let secret = StaticSecret::from(identity.x25519_secret());
        let public = b64(PublicKey::from(&secret).as_bytes());
        Self {
            identity,
            secret,
            public,
        }
    }

    pub(crate) fn device_id(&self) -> &str {
        &self.identity.device_id
    }

    /// `PUT /devices/self/x25519`
    pub(crate) fn registration(&self) -> DeviceX25519 {
        DeviceX25519 {
            public: self.public.clone(),
            signature: self.identity.sign(x25519_message(&self.public).as_bytes()),
        }
    }

    /// `frame_key` (ID `key_id`) of `participant` sealed to device
    /// `device` with X25519 key `x25519`.
    pub(crate) fn seal(
        &self,
        scope: &str,
        participant: u32,
        device: &str,
        x25519: &str,
        key_id: u8,
        frame_key: &[u8; FRAME_KEY],
    ) -> Result<VoiceKeyBox> {
        let recipient = key32(x25519)?;
        let ephemeral = StaticSecret::from(rand::random::<[u8; 32]>());
        let ephemeral_public = *PublicKey::from(&ephemeral).as_bytes();
        let shared = ephemeral.diffie_hellman(&PublicKey::from(recipient));
        let (key, nonce) = derive(shared.as_bytes(), &ephemeral_public, &recipient);
        let mut plain = Vec::with_capacity(FRAME_KEY + 5);
        plain.extend_from_slice(frame_key);
        plain.push(key_id);
        plain.extend_from_slice(&participant.to_be_bytes());
        let sealed = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| anyhow!("key"))?
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: &plain, aad: scope.as_bytes() })
            .map_err(|_| anyhow!("sealing failed"))?;
        let (ephemeral, data) = (b64(&ephemeral_public), b64(&sealed));
        let signature = self
            .identity
            .sign(key_box_message(scope, participant, device, &ephemeral, &data).as_bytes());
        Ok(VoiceKeyBox {
            device_id: device.to_owned(),
            ephemeral,
            data,
            signature,
        })
    }

    /// The frame key in `push`, which the device with Ed25519 key `sender`
    /// signed; `(key, key_id)`.
    pub(crate) fn open(&self, scope: &str, push: &VoiceKeyPush, sender: &str) -> Result<([u8; FRAME_KEY], u8)> {
        let message = key_box_message(scope, push.participant, self.device_id(), &push.ephemeral, &push.data);
        verify(sender, message.as_bytes(), &push.signature).map_err(|_| anyhow!("key box not signed by its sender"))?;
        let ephemeral = key32(&push.ephemeral)?;
        let shared = self.secret.diffie_hellman(&PublicKey::from(ephemeral));
        let mine = *PublicKey::from(&self.secret).as_bytes();
        let (key, nonce) = derive(shared.as_bytes(), &ephemeral, &mine);
        let sealed = unb64(&push.data).map_err(|_| anyhow!("invalid key box"))?;
        let plain = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| anyhow!("key"))?
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: &sealed, aad: scope.as_bytes() })
            .map_err(|_| anyhow!("key box does not open"))?;
        ensure!(plain.len() == FRAME_KEY + 5, "invalid key box");
        let key_id = plain[FRAME_KEY];
        let participant = u32::from_be_bytes(plain[FRAME_KEY + 1..].try_into().expect("4 bytes"));
        ensure!(participant == push.participant && key_id == push.key_id, "key box for someone else");
        let mut frame_key = [0u8; FRAME_KEY];
        frame_key.copy_from_slice(&plain[..FRAME_KEY]);
        Ok((frame_key, key_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str) -> DeviceKeys {
        DeviceKeys::new(ClientIdentity::new("install", id, &rand::random::<[u8; 32]>()).unwrap())
    }

    #[test]
    fn a_sealed_key_opens_for_its_device_alone() {
        let (alice, bob, carol) = (device("a"), device("b"), device("c"));
        let key = rand::random::<[u8; FRAME_KEY]>();
        let sealed = alice.seal("42", 7, "b", &bob.public, 3, &key).unwrap();
        let push = VoiceKeyPush {
            channel_id: farwing_protocol::chat::Id(42),
            participant: 7,
            device_id: "a".into(),
            key_id: 3,
            ephemeral: sealed.ephemeral.clone(),
            data: sealed.data.clone(),
            signature: sealed.signature.clone(),
        };
        let alice_ed = alice.identity.public_key();
        assert_eq!(bob.open("42", &push, &alice_ed).unwrap(), (key, 3));
        // Another device, scope, sender or number: refused.
        assert!(carol.open("42", &push, &alice_ed).is_err());
        assert!(bob.open("43", &push, &alice_ed).is_err());
        assert!(bob.open("42", &push, &carol.identity.public_key()).is_err());
        let mut other = push.clone();
        other.participant = 8;
        assert!(bob.open("42", &other, &alice_ed).is_err());
        // The registration verifies with the device key.
        let reg = bob.registration();
        assert!(verify(&bob.identity.public_key(), x25519_message(&reg.public).as_bytes(), &reg.signature).is_ok());
    }
}
