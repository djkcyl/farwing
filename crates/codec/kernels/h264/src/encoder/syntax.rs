// SPDX-License-Identifier: LGPL-2.1-or-later
//! Progressive, single-reference AVC syntax. No reorder or asynchronous output.
use super::{Config, EncodeError, Result};
use crate::tables;
/// Byte-reservoir writer: one bounded append per syntax element, not per bit.
pub(super) struct BitWriter {
    bytes: Vec<u8>,
    pending: u64,
    bits: u32,
}
impl BitWriter {
    pub fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(4096),
            pending: 0,
            bits: 0,
        }
    }
    #[inline]
    pub fn u(&mut self, n: u32, v: u32) {
        debug_assert!(n <= 32);
        self.pending = (self.pending << n) | (u64::from(v) & ((1u64 << n) - 1));
        self.bits += n;
        // Like the mature encoder bit writers, commit one word at a time.
        // Before append fewer than 32 bits remain; n<=32 keeps the reservoir
        // within 63 bits. Consumed upper bits are discarded by the u32 cast.
        if self.bits >= 32 {
            self.bits -= 32;
            self.bytes
                .extend_from_slice(&((self.pending >> self.bits) as u32).to_be_bytes());
        }
    }
    #[inline]
    pub fn ue(&mut self, v: u32) {
        let value = v + 1;
        let zeros = 31 - value.leading_zeros();
        self.u(zeros, 0);
        self.u(zeros + 1, value);
    }
    #[inline]
    pub fn se(&mut self, v: i32) {
        self.ue(if v <= 0 {
            v.unsigned_abs() * 2
        } else {
            v as u32 * 2 - 1
        });
    }
    pub fn position(&self) -> usize {
        self.bytes.len() * 8 + self.bits as usize
    }
    pub fn rbsp_trailing_bits(&mut self) {
        self.u(1, 1);
        if self.bits % 8 > 0 {
            self.u(8 - self.bits % 8, 0);
        }
    }
    pub fn into_bytes(mut self) -> Vec<u8> {
        debug_assert_eq!(self.bits % 8, 0);
        while self.bits > 0 {
            self.bits -= 8;
            self.bytes.push((self.pending >> self.bits) as u8);
        }
        self.bytes
    }
}
fn encode_level(w: &mut BitWriter, sl: u32, first: bool, level: i32) -> Result<u32> {
    let code = if level > 0 {
        i64::from(level) * 2 - 2
    } else {
        -i64::from(level) * 2 - 1
    } - if first { 2 } else { 0 };
    if code < 0 {
        return Err(EncodeError("CAVLC level underflow"));
    }
    let (prefix, suffix, n) = if sl == 0 && code < 14 {
        (code as u32, 0, 0)
    } else if sl == 0 && code < 30 {
        (14, (code - 14) as u32, 4)
    } else if sl > 0 && (code >> sl) < 15 {
        ((code >> sl) as u32, (code & ((1 << sl) - 1)) as u32, sl)
    } else {
        let base = if sl == 0 { 30 } else { 15 << sl };
        let mut p = 15;
        loop {
            let n = p - 3;
            let extra = if p >= 16 { (1i64 << n) - 4096 } else { 0 };
            if code < base + extra + (1i64 << n) {
                break (p, (code - base - extra) as u32, n);
            }
            p += 1;
            if p > 31 {
                return Err(EncodeError("CAVLC level overflow"));
            }
        }
    };
    let length = prefix + 1 + n;
    let bits = (1 << n) | suffix;
    if length <= 32 {
        w.u(length, bits);
    } else {
        w.u(length - 32, 0);
        w.u(32, bits);
    }
    let next = sl.max(1);
    Ok(next + u32::from(next < 6 && level.unsigned_abs() > (3 << (next - 1))))
}

pub(super) const SCAN: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
pub(super) const BLOCKS: [(usize, usize); 16] = [
    (0, 0),
    (4, 0),
    (0, 4),
    (4, 4),
    (8, 0),
    (12, 0),
    (8, 4),
    (12, 4),
    (0, 8),
    (4, 8),
    (0, 12),
    (4, 12),
    (8, 8),
    (12, 8),
    (8, 12),
    (12, 12),
];
const CBP_INTER: [u8; 48] = [
    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44, 33, 34,
    36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];
const CBP_INTRA: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

pub(super) fn nal(out: &mut Vec<u8>, header: u8, w: BitWriter) {
    let bytes = w.into_bytes();
    out.reserve(bytes.len() + 5);
    out.extend_from_slice(&[0, 0, 0, 1, header]);
    let mut zeros = 0;
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if zeros == 2 && b <= 3 {
            out.extend_from_slice(&bytes[start..i]);
            out.push(3);
            start = i;
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out.extend_from_slice(&bytes[start..]);
}
pub(super) fn parameters(c: Config, out: &mut Vec<u8>) {
    let mw = c.width.div_ceil(16);
    let mh = c.height.div_ceil(16);
    let fs = mw * mh;
    // Initialize from effective FPS and bitrate;
    // SetRates retains this SPS until the encoder is recreated.
    const LIMITS: [(u32, u32, u32, u32, u32); 17] = [
        (10, 1485, 99, 396, 64),
        (9, 1485, 99, 396, 128),
        (11, 3000, 396, 900, 192),
        (12, 6000, 396, 2376, 384),
        (13, 11880, 396, 2376, 768),
        (20, 11880, 396, 2376, 2000),
        (21, 19800, 792, 4752, 4000),
        (22, 20250, 1620, 8100, 4000),
        (30, 40500, 1620, 8100, 10000),
        (31, 108000, 3600, 18000, 14000),
        (32, 216000, 5120, 20480, 20000),
        (40, 245760, 8192, 32768, 20000),
        (41, 245760, 8192, 32768, 50000),
        (42, 522240, 8704, 34816, 50000),
        (50, 589824, 22080, 110400, 135000),
        (51, 983040, 36864, 184320, 240000),
        (52, 2073600, 36864, 184320, 240000),
    ];
    let mbps = (fs as f32 * c.fps.min(60) as f32) as u32;
    let level = LIMITS
        .iter()
        .find(|&&(_, rate, area, dpb, bitrate)| {
            rate >= mbps
                && area >= fs
                && area * 8 >= mw * mw
                && area * 8 >= mh * mh
                && dpb >= fs * MAX_REFERENCES
                && bitrate * 1200 >= c.bitrate
        })
        .map_or(51, |row| row.0);
    let mut w = BitWriter::new();
    w.u(8, 66);
    w.u(8, if level == 9 { 0xd0 } else { 0xc0 });
    w.u(8, if level == 9 { 11 } else { level });
    w.ue(0);
    w.ue(11); // 15-bit frame_num.
    w.ue(0); // POC type 0, 16-bit pic_order_cnt_lsb.
    w.ue(12);
    // One short-term and two long-term references (loss recovery).
    w.ue(MAX_REFERENCES);
    w.u(1, 0);
    w.ue(mw - 1);
    w.ue(mh - 1);
    w.u(1, 1);
    w.u(1, 1);
    let crop = mw * 16 != c.width || mh * 16 != c.height;
    w.u(1, crop as u32);
    if crop {
        w.ue(0);
        w.ue((mw * 16 - c.width) / 2);
        w.ue(0);
        w.ue((mh * 16 - c.height) / 2);
    }
    w.u(1, 1); // VUI; actual input timestamps, no fixed-frame-rate assertion.
    w.u(1, 0);
    w.u(1, 0);
    w.u(1, 1);
    w.u(3, 5);
    w.u(1, 0);
    w.u(1, 1);
    w.u(8, 6);
    w.u(8, 4);
    w.u(8, 6); // SMPTE170M / gamma2.2 / BT601, limited.
    w.u(1, 0);
    w.u(1, 0);
    w.u(1, 0);
    w.u(1, 0);
    w.u(1, 0);
    w.u(1, 1);
    w.u(1, 1);
    w.ue(0);
    w.ue(0);
    w.ue(12);
    w.ue(12);
    w.ue(0);
    w.ue(MAX_REFERENCES);
    w.rbsp_trailing_bits();
    nal(out, 0x67, w);
    let mut w = BitWriter::new();
    w.ue(0);
    w.ue(0);
    w.u(1, 0);
    w.u(1, 0);
    w.ue(0);
    w.ue(0);
    w.ue(0);
    w.u(1, 0);
    w.u(2, 0);
    w.se(0);
    w.se(0);
    w.se(0);
    w.u(1, 1);
    w.u(1, 0);
    w.u(1, 0);
    w.rbsp_trailing_bits();
    nal(out, 0x68, w);
}
/// `max_num_ref_frames`: the previous picture and two long-term references.
pub(super) const MAX_REFERENCES: u32 = 3;

/// Long-term reference syntax of one slice.
#[derive(Clone, Copy, Default)]
pub(super) struct SliceReferences {
    /// IDR: keep this picture as long-term reference 0.
    pub long_term_idr: bool,
    /// P: predict only from long-term reference `slot`.
    pub predict_from: Option<u8>,
    /// P: keep the previous picture as long-term reference `slot`.
    pub keep_previous: Option<u8>,
}

pub(super) fn slice(idr: bool, frame: u16, id: u16, qp: i32, refs: SliceReferences) -> BitWriter {
    let mut w = BitWriter::new();
    w.ue(0);
    w.ue(if idr { 7 } else { 5 });
    w.ue(0);
    w.u(15, frame as u32);
    if idr {
        w.ue(id as u32);
    }
    w.u(16, u32::from(frame) * 2);
    if idr {
        w.u(1, 0); // no_output_of_prior_pics_flag
        w.u(1, u32::from(refs.long_term_idr));
    } else {
        w.u(1, 0); // num_ref_idx_active_override_flag: one reference
        match refs.predict_from {
            Some(slot) => {
                w.u(1, 1); // ref_pic_list_modification_flag_l0
                w.ue(2); // long_term_pic_num follows
                w.ue(u32::from(slot));
                w.ue(3);
            }
            None => w.u(1, 0),
        }
        match refs.keep_previous {
            Some(slot) => {
                w.u(1, 1); // adaptive_ref_pic_marking_mode_flag
                w.ue(4); // max_long_term_frame_idx_plus1
                w.ue(2);
                w.ue(3); // previous picture (difference 1) → long-term `slot`
                w.ue(0);
                w.ue(u32::from(slot));
                w.ue(0);
            }
            None => w.u(1, 0),
        }
    }
    w.se(qp - 26);
    w.ue(0);
    w.se(0);
    w.se(0);
    w
}
pub(super) fn cbp(w: &mut BitWriter, intra: bool, value: u8) {
    const fn reverse(map: [u8; 48]) -> [u8; 48] {
        let mut out = [0; 48];
        let mut i = 0;
        while i < 48 {
            out[map[i] as usize] = i as u8;
            i += 1;
        }
        out
    }
    const INTRA: [u8; 48] = reverse(CBP_INTRA);
    const INTER: [u8; 48] = reverse(CBP_INTER);
    w.ue(if intra {
        INTRA[value as usize]
    } else {
        INTER[value as usize]
    } as u32);
}

pub(super) fn residual(w: &mut BitWriter, coeff: &[i16], nc: i32) -> Result<()> {
    let mut mask = super::pixels::nonzero_mask(coeff);
    let total = mask.count_ones() as usize;
    if total == 0 {
        let (len, bits) = if coeff.len() == 4 {
            (tables::CHROMA_TOKEN_LEN[0], tables::CHROMA_TOKEN_BITS[0])
        } else {
            let table = if nc < 2 {
                0
            } else if nc < 4 {
                1
            } else if nc < 8 {
                2
            } else {
                3
            };
            (
                tables::COEFF_TOKEN_LEN[table * 68],
                tables::COEFF_TOKEN_BITS[table * 68],
            )
        };
        w.u(len as u32, bits as u32);
        return Ok(());
    }
    let mut levels = [0i16; 16];
    let mut positions = [0u8; 16];
    for i in 0..total {
        let at = 31 - mask.leading_zeros();
        levels[i] = coeff[at as usize];
        positions[i] = at as u8;
        mask ^= 1 << at;
    }
    let trailing = levels[..total]
        .iter()
        .take(3)
        .take_while(|&&v| v.abs() == 1)
        .count();
    let entry = total * 4 + trailing;
    let (len, bits) = if coeff.len() == 4 {
        (
            tables::CHROMA_TOKEN_LEN[entry],
            tables::CHROMA_TOKEN_BITS[entry],
        )
    } else {
        let table = if nc < 2 {
            0
        } else if nc < 4 {
            1
        } else if nc < 8 {
            2
        } else {
            3
        };
        let at = table * 68 + entry;
        (tables::COEFF_TOKEN_LEN[at], tables::COEFF_TOKEN_BITS[at])
    };
    if len == 0 {
        return Err(EncodeError("invalid CAVLC token"));
    }
    w.u(len as u32, bits as u32);
    let signs = levels[..trailing]
        .iter()
        .fold(0, |bits, &v| (bits << 1) | u32::from(v < 0));
    w.u(trailing as u32, signs);
    let mut suffix = u32::from(total > 10 && trailing < 3);
    for (i, &v) in levels[..total].iter().enumerate().skip(trailing) {
        suffix = encode_level(w, suffix, i == trailing && trailing < 3, i32::from(v))
            .map_err(|_| EncodeError("CAVLC level overflow"))?;
    }
    let mut zeros = positions[0] as usize + 1 - total;
    if total < coeff.len() {
        let (len, bits) = if coeff.len() == 4 {
            let at = (total - 1) * 4 + zeros;
            (tables::CHROMA_ZEROS_LEN[at], tables::CHROMA_ZEROS_BITS[at])
        } else {
            let at = (total - 1) * 16 + zeros;
            (tables::TOTAL_ZEROS_LEN[at], tables::TOTAL_ZEROS_BITS[at])
        };
        w.u(len as u32, bits as u32);
    }
    for i in 0..total - 1 {
        if zeros == 0 {
            break;
        }
        let run = (positions[i] - positions[i + 1] - 1) as usize;
        let at = (zeros.min(7) - 1) * 16 + run;
        w.u(tables::RUN_LEN[at] as u32, tables::RUN_BITS[at] as u32);
        zeros -= run;
    }
    Ok(())
}
