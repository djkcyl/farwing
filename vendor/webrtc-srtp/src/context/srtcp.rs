use bytes::Bytes;
use util::marshal::*;

use super::*;
use crate::error::Result;

impl Context {
    /// DecryptRTCP decrypts a RTCP packet with an encrypted payload
    pub fn decrypt_rtcp(&mut self, encrypted: &[u8]) -> Result<Bytes> {
        // Header parsing alone only checks four bytes. Validate SSRC, index
        // and the negotiated tag before any cipher-specific tail indexing.
        let minimum = 8
            + crate::key_derivation::SRTCP_INDEX_SIZE
            + self.cipher.rtcp_auth_tag_len()
            + self.cipher.aead_auth_tag_len();
        if encrypted.len() < minimum {
            return Err(Error::SrtcpTooSmall(encrypted.len(), minimum));
        }
        let mut buf = encrypted;
        rtcp::header::Header::unmarshal(&mut buf)?;

        let index = self.cipher.get_rtcp_index(encrypted);
        let ssrc = u32::from_be_bytes([encrypted[4], encrypted[5], encrypted[6], encrypted[7]]);

        // New SSRCs remain provisional until authentication succeeds. Failed
        // packets must not accumulate receive state or anchor its replay window.
        let mut provisional = None;
        let state = match self.srtcp_ssrc_states.get_mut(&ssrc) {
            Some(state) => state,
            None => provisional.insert(SrtcpSsrcState {
                ssrc,
                replay_detector: Some((self.new_srtcp_replay_detector)()),
                ..Default::default()
            }),
        };
        if let Some(replay_detector) = &mut state.replay_detector {
            if !replay_detector.check(index as u64) {
                return Err(Error::SrtcpSsrcDuplicated(ssrc, index));
            }
        }

        let dst = self.cipher.decrypt_rtcp(encrypted, index, ssrc)?;

        if let Some(replay_detector) = &mut state.replay_detector {
            replay_detector.accept();
        }
        if let Some(state) = provisional {
            self.srtcp_ssrc_states.insert(ssrc, state);
        }

        Ok(dst)
    }

    /// EncryptRTCP marshals and encrypts an RTCP packet, writing to the dst buffer provided.
    /// If the dst buffer does not have the capacity to hold `len(plaintext) + 14` bytes, a new one will be allocated and returned.
    pub fn encrypt_rtcp(&mut self, decrypted: &[u8]) -> Result<Bytes> {
        if decrypted.len() < 8 {
            return Err(Error::ErrTooShortRtcp);
        }

        let mut buf = decrypted;
        rtcp::header::Header::unmarshal(&mut buf)?;

        let ssrc = u32::from_be_bytes([decrypted[4], decrypted[5], decrypted[6], decrypted[7]]);

        let index = {
            let state = self.get_srtcp_ssrc_state(ssrc);
            if state.srtcp_index >= MAX_SRTCP_INDEX {
                return Err(Error::ErrExceededMaxPackets);
            }
            state.srtcp_index += 1;
            state.srtcp_index
        };

        self.cipher.encrypt_rtcp(decrypted, index, ssrc)
    }
}
