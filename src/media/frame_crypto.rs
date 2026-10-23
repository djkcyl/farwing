//! End-to-end encryption of forwarded video and audio frames
//! (`docs/protocol/deployment-design.md` §5.3, `farwing-protocol.md` §6.8).
//!
//! The host encrypts each encoded frame before packetization; the forwarder
//! passes ciphertext; viewers decrypt after the frame is assembled. Only what
//! packetizers and receivers read stays in the clear: NAL unit headers,
//! parameter sets and the first bytes of each slice (the receiver reads the
//! picture parameter set ID there) for H.264 and H.265; OBU headers and
//! sequence headers for AV1; the TOC byte of an Opus frame (the receiver
//! reads the frame's duration there).
//!
//! An encrypted unit is `clear ‖ AES-128-GCM(rest) ‖ tag(16) ‖ counter(8) ‖
//! key ID(1) ‖ 0xFE`, authenticated with the clear part as associated
//! data; the nonce is the key's 4-byte salt followed by the counter. H.264
//! and H.265 units are then escaped like any NAL unit payload so they carry
//! no start code; AV1 units get their new size; an Opus frame is one unit.

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce, Tag};
use anyhow::{Context, Result, bail, ensure};

/// Key material: AES-128 key and nonce salt.
pub(crate) const KEY_BYTES: usize = 20;
/// Slice bytes after the NAL header left in the clear.
const CLEAR_SLICE: usize = 8;
const TRAILER: usize = 16 + 8 + 1 + 1;
const MARK: u8 = 0xFE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Codec {
    H264,
    H265,
    Av1,
    Opus,
}

#[derive(Clone)]
pub(crate) struct FrameKey {
    pub id: u8,
    cipher: Aes128Gcm,
    salt: [u8; 4],
}

impl FrameKey {
    pub(crate) fn new(id: u8, material: &[u8]) -> Result<Self> {
        ensure!(
            material.len() == KEY_BYTES,
            "frame key has {} bytes",
            material.len()
        );
        Ok(Self {
            id,
            cipher: Aes128Gcm::new_from_slice(&material[..16]).context("frame key")?,
            salt: material[16..].try_into().expect("4 bytes"),
        })
    }

    /// New random key material.
    pub(crate) fn generate() -> [u8; KEY_BYTES] {
        rand::random()
    }

    fn nonce(&self, counter: u64) -> Nonce<aes_gcm::aead::consts::U12> {
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&self.salt);
        nonce[4..].copy_from_slice(&counter.to_be_bytes());
        Nonce::from(nonce)
    }
}

/// The host's side: one counter across every unit it encrypts.
pub(crate) struct Encryptor {
    key: FrameKey,
    counter: u64,
}

impl Encryptor {
    pub(crate) fn new(key: FrameKey) -> Self {
        Self {
            key,
            counter: rand::random::<u32>().into(),
        }
    }

    pub(crate) fn set_key(&mut self, key: FrameKey) {
        self.key = key;
    }

    /// `frame` (Annex B or sized OBUs) encrypted.
    pub(crate) fn encrypt(&mut self, codec: Codec, frame: &[u8]) -> Result<Vec<u8>> {
        match codec {
            Codec::H264 | Codec::H265 => self.encrypt_annex_b(codec, frame),
            Codec::Av1 => self.encrypt_av1(frame),
            Codec::Opus => {
                ensure!(!frame.is_empty(), "empty Opus frame");
                let mut out = frame[..1].to_vec();
                out.extend_from_slice(&self.seal(&frame[..1], &frame[1..])?);
                Ok(out)
            }
        }
    }

    fn seal(&mut self, clear: &[u8], rest: &[u8]) -> Result<Vec<u8>> {
        self.counter = self.counter.wrapping_add(1);
        let mut sealed = Vec::with_capacity(rest.len() + TRAILER);
        sealed.extend_from_slice(rest);
        let tag = self
            .key
            .cipher
            .encrypt_in_place_detached(&self.key.nonce(self.counter), clear, &mut sealed)
            .map_err(|_| anyhow::anyhow!("frame encryption failed"))?;
        sealed.extend_from_slice(&tag);
        sealed.extend_from_slice(&self.counter.to_be_bytes());
        sealed.push(self.key.id);
        sealed.push(MARK);
        Ok(sealed)
    }

    fn encrypt_annex_b(&mut self, codec: Codec, frame: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(frame.len() + 64);
        for unit in crate::media::video_format::annex_b_units(frame) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            let header = header_len(codec);
            if !encrypted_nal(codec, unit) {
                out.extend_from_slice(unit);
                continue;
            }
            let clear = &unit[..header + CLEAR_SLICE];
            let sealed = self.seal(clear, &unit[header + CLEAR_SLICE..])?;
            out.extend_from_slice(clear);
            escape(clear, &sealed, &mut out);
        }
        Ok(out)
    }

    fn encrypt_av1(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(frame.len() + 64);
        for obu in obus(frame)? {
            if !matches!(obu.kind, 4 | 6) || obu.payload.is_empty() {
                out.extend_from_slice(obu.bytes);
                continue;
            }
            let sealed = self.seal(obu.header, obu.payload)?;
            out.extend_from_slice(obu.header);
            put_leb(sealed.len(), &mut out);
            out.extend_from_slice(&sealed);
        }
        Ok(out)
    }
}

/// A viewer's side: the current and the previous key.
#[derive(Default)]
pub(crate) struct Decryptor {
    keys: Vec<FrameKey>,
}

impl Decryptor {
    /// Accept `key` (newest last); the two newest are kept.
    pub(crate) fn add_key(&mut self, key: FrameKey) {
        self.keys.retain(|k| k.id != key.id);
        self.keys.push(key);
        if self.keys.len() > 2 {
            self.keys.remove(0);
        }
    }

    fn open(&self, clear: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            sealed.len() >= TRAILER && sealed[sealed.len() - 1] == MARK,
            "not an encrypted unit"
        );
        let body = sealed.len() - TRAILER;
        let tag = Tag::from_slice(&sealed[body..body + 16]);
        let counter = u64::from_be_bytes(sealed[body + 16..body + 24].try_into().expect("8 bytes"));
        let id = sealed[body + 24];
        let key = self
            .keys
            .iter()
            .find(|k| k.id == id)
            .with_context(|| format!("no frame key {id}"))?;
        let mut plain = sealed[..body].to_vec();
        key.cipher
            .decrypt_in_place_detached(&key.nonce(counter), clear, &mut plain, tag)
            .map_err(|_| anyhow::anyhow!("frame authentication failed"))?;
        Ok(plain)
    }

    /// `frame` as the host encoded it.
    pub(crate) fn decrypt(&self, codec: Codec, frame: &[u8]) -> Result<Vec<u8>> {
        match codec {
            Codec::H264 | Codec::H265 => {
                let mut out = Vec::with_capacity(frame.len());
                for unit in crate::media::video_format::annex_b_units(frame) {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    let header = header_len(codec);
                    if !encrypted_nal(codec, unit) {
                        out.extend_from_slice(unit);
                        continue;
                    }
                    let clear = &unit[..header + CLEAR_SLICE];
                    let sealed = unescape(clear, &unit[header + CLEAR_SLICE..]);
                    out.extend_from_slice(clear);
                    out.extend_from_slice(&self.open(clear, &sealed)?);
                }
                Ok(out)
            }
            Codec::Av1 => {
                let mut out = Vec::with_capacity(frame.len());
                for obu in obus(frame)? {
                    if !matches!(obu.kind, 4 | 6) || obu.payload.is_empty() {
                        out.extend_from_slice(obu.bytes);
                        continue;
                    }
                    let plain = self.open(obu.header, obu.payload)?;
                    out.extend_from_slice(obu.header);
                    put_leb(plain.len(), &mut out);
                    out.extend_from_slice(&plain);
                }
                Ok(out)
            }
            Codec::Opus => {
                ensure!(!frame.is_empty(), "empty Opus frame");
                let mut out = frame[..1].to_vec();
                out.extend_from_slice(&self.open(&frame[..1], &frame[1..])?);
                Ok(out)
            }
        }
    }
}

fn header_len(codec: Codec) -> usize {
    if codec == Codec::H264 { 1 } else { 2 }
}

/// Slices long enough to hide anything: H.264 types 1–5, H.265 VCL types.
fn encrypted_nal(codec: Codec, unit: &[u8]) -> bool {
    let header = header_len(codec);
    if unit.len() <= header + CLEAR_SLICE {
        return false;
    }
    match codec {
        Codec::H264 => (1..=5).contains(&(unit[0] & 31)),
        Codec::H265 => (unit[0] >> 1) & 63 < 32,
        Codec::Av1 | Codec::Opus => false,
    }
}

/// Appends `data` escaped (emulation prevention), continuing after `before`.
fn escape(before: &[u8], data: &[u8], out: &mut Vec<u8>) {
    let mut zeros = before.iter().rev().take_while(|b| **b == 0).count().min(2);
    for &byte in data {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
}

/// Removes the escaping of [`escape`].
fn unescape(before: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = before.iter().rev().take_while(|b| **b == 0).count().min(2);
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

struct Obu<'a> {
    kind: u8,
    /// Header and extension, without the size.
    header: &'a [u8],
    payload: &'a [u8],
    /// The whole OBU.
    bytes: &'a [u8],
}

fn obus(data: &[u8]) -> Result<Vec<Obu<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let start = at;
        let first = data[at];
        if first & 0x80 != 0 || first & 2 == 0 {
            bail!("AV1 OBU without size field");
        }
        let header_end = at + 1 + usize::from(first & 4 != 0);
        ensure!(header_end <= data.len(), "AV1 OBU truncated");
        let mut size_at = header_end;
        let size = crate::media::av1::leb(data, &mut size_at)?;
        let end = size_at.checked_add(size).context("AV1 OBU size")?;
        ensure!(end <= data.len(), "AV1 OBU truncated");
        out.push(Obu {
            kind: (first >> 3) & 15,
            header: &data[start..header_end],
            payload: &data[size_at..end],
            bytes: &data[start..end],
        });
        at = end;
    }
    Ok(out)
}

fn put_leb(mut value: usize, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push(value as u8 | 128);
        value >>= 7;
    }
    out.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: u8) -> FrameKey {
        FrameKey::new(id, &[id; KEY_BYTES]).unwrap()
    }

    fn h264_frame() -> Vec<u8> {
        let mut frame = vec![0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0x8C, 0x8D, 0x40];
        frame.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]);
        frame.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]);
        // A slice whose bytes would contain start codes once encrypted.
        frame.extend((0..4000u32).map(|i| (i * 7 % 251) as u8 | 1));
        frame
    }

    #[test]
    fn h264_frames_round_trip_and_keep_their_structure() {
        let frame = h264_frame();
        let mut sender = Encryptor::new(key(1));
        let sealed = sender.encrypt(Codec::H264, &frame).unwrap();
        let units = crate::media::video_format::annex_b_units(&sealed);
        // Parameter sets and the slice header stay readable.
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], &frame[4..11]);
        assert_eq!(units[2][0], 0x65);
        assert_eq!(
            &units[2][..1 + CLEAR_SLICE],
            &frame[23..23 + 1 + CLEAR_SLICE]
        );
        // The payload is not the plain one.
        assert!(!sealed.windows(64).any(|w| frame[100..164] == *w));
        let mut viewer = Decryptor::default();
        viewer.add_key(key(1));
        assert_eq!(
            viewer.decrypt(Codec::H264, &sealed).unwrap()[4..],
            frame[4..]
        );
        // Any change to the clear header or ciphertext is detected.
        let mut tampered = sealed.clone();
        let at = sealed.len() - 100;
        tampered[at] ^= 1;
        assert!(viewer.decrypt(Codec::H264, &tampered).is_err());
        // Without the key: refused.
        assert!(Decryptor::default().decrypt(Codec::H264, &sealed).is_err());
        // The packetizer still splits it into the same units.
        let mut packetizer = webrtc::rtp::codecs::h264::H264Payloader::default();
        use webrtc::rtp::packetizer::Payloader;
        assert!(
            !packetizer
                .payload(1200, &bytes::Bytes::from(sealed))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn h265_frames_round_trip() {
        let mut frame = vec![0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01];
        frame.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01]);
        frame.extend((0..3000u32).map(|i| (i % 256) as u8));
        frame.push(0x80);
        let mut sender = Encryptor::new(key(2));
        let sealed = sender.encrypt(Codec::H265, &frame).unwrap();
        let mut viewer = Decryptor::default();
        viewer.add_key(key(2));
        let plain = viewer.decrypt(Codec::H265, &sealed).unwrap();
        assert_eq!(
            crate::media::video_format::annex_b_units(&plain),
            crate::media::video_format::annex_b_units(&frame)
        );
        assert!(crate::features::host::hevc::payloads(1200, &sealed).is_ok());
    }

    #[test]
    fn av1_frames_round_trip_with_new_sizes() {
        // Sequence header (clear), frame OBU (encrypted).
        let mut frame = vec![0x0A, 3, 1, 2, 3];
        frame.push(0x32);
        let payload: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        put_leb(payload.len(), &mut frame);
        frame.extend_from_slice(&payload);
        let mut sender = Encryptor::new(key(3));
        let sealed = sender.encrypt(Codec::Av1, &frame).unwrap();
        let units: Vec<_> = obus(&sealed).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].bytes, &frame[..5]);
        assert_eq!(units[1].payload.len(), payload.len() + TRAILER);
        let mut viewer = Decryptor::default();
        viewer.add_key(key(3));
        assert_eq!(viewer.decrypt(Codec::Av1, &sealed).unwrap(), frame);
        assert!(crate::media::av1::payloads(1200, &sealed, true).is_ok());
    }

    #[test]
    fn viewers_keep_the_previous_key_for_a_while() {
        let mut viewer = Decryptor::default();
        viewer.add_key(key(1));
        viewer.add_key(key(2));
        let mut old = Encryptor::new(key(1));
        let frame = h264_frame();
        let sealed = old.encrypt(Codec::H264, &frame).unwrap();
        assert!(viewer.decrypt(Codec::H264, &sealed).is_ok());
        viewer.add_key(key(3));
        assert!(viewer.decrypt(Codec::H264, &sealed).is_err());
    }

    #[test]
    fn escaping_round_trips_after_any_prefix() {
        let data = [0u8, 0, 0, 1, 0, 0, 2, 0, 0, 3, 3, 0, 0];
        for before in [&[][..], &[0][..], &[0, 0][..], &[5, 0][..]] {
            let mut out = Vec::new();
            escape(before, &data, &mut out);
            let mut joined = before.to_vec();
            joined.extend_from_slice(&out);
            assert!(
                !joined.windows(3).any(|w| w == [0, 0, 1] || w == [0, 0, 0]),
                "{before:?}"
            );
            assert_eq!(unescape(before, &out), data);
        }
    }

    /// The frames of `server/web/src/test/frameCrypto.test.ts`: the browser
    /// decrypts exactly what this side encrypts.
    #[test]
    fn known_vectors() {
        let material: Vec<u8> = (0..KEY_BYTES as u8).collect();
        let encryptor = || Encryptor {
            key: FrameKey::new(3, &material).unwrap(),
            counter: 41,
        };
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let slice: Vec<u8> = (0..40u8).map(|i| i.wrapping_mul(37)).collect();
        let mut h264 = vec![0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F, 0, 0, 0, 1, 0x65];
        h264.extend_from_slice(&slice);
        let mut h265 = vec![0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01, 0, 0, 0, 1, 0x26, 0x01];
        h265.extend_from_slice(&slice);
        let mut av1 = vec![0x12, 0x00, 0x0A, 0x03, 0x00, 0x00, 0x00, 0x32, 40];
        av1.extend_from_slice(&slice);
        let mut opus = vec![0x78];
        opus.extend_from_slice(&slice);
        let vectors = [
            (
                Codec::H264,
                h264,
                "000000016742c01f000000016500254a6f94b9de0364edec74a6515bd1d4b30df60eaeb7ab598473def5235b028a47ddefcfc27980e1b382233bbe126eb1168baf42e6023e000003000003000003002a03fe",
            ),
            (
                Codec::H265,
                h265,
                "0000000140010c0100000001260100254a6f94b9de0364edec74a6515bd1d4b30df60eaeb7ab598473def5235b028a47ddefcfc279809172c12615cd47a922cac65a105864f3000003000003000003002a03fe",
            ),
            (
                Codec::Av1,
                av1,
                "12000a0300000032424c85d48c8e0983f9ac8be5de56469fd3716c2b861d1b232a521ff507f7aa5158f9430f863a67beccab52cf104a7c268e7254887dec023087000000000000002a03fe",
            ),
            (
                Codec::Opus,
                opus,
                "784c85d48c8e0983f9ac8be5de56469fd3716c2b861d1b232a521ff507f7aa5158f9430f863a67becc375db3177f6f76fb2798de01c904d44b000000000000002a03fe",
            ),
        ];
        for (codec, frame, expected) in vectors {
            let sealed = encryptor().encrypt(codec, &frame).unwrap();
            assert_eq!(hex(&sealed), expected, "{codec:?}");
        }
    }

    #[test]
    fn opus_frames_keep_their_toc_byte() {
        let frame: Vec<u8> = (0..120u8).collect();
        let mut encryptor = Encryptor::new(key(4));
        let sealed = encryptor.encrypt(Codec::Opus, &frame).unwrap();
        assert_eq!(sealed[0], frame[0]);
        assert_ne!(&sealed[1..frame.len()], &frame[1..]);
        let mut decryptor = Decryptor::default();
        decryptor.add_key(key(4));
        assert_eq!(decryptor.decrypt(Codec::Opus, &sealed).unwrap(), frame);
        assert_eq!(
            decryptor
                .decrypt(
                    Codec::Opus,
                    &encryptor.encrypt(Codec::Opus, &[0x78]).unwrap()
                )
                .unwrap(),
            [0x78]
        );
    }
}
