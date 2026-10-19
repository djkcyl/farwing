// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg 8.0.3 h264_cavlc.c, Copyright (c) 2003 Michael Niedermayer.
// Two-stage prefix tables, bounded suffix parsing and direct scanned coefficient
// storage. One reusable table set serves every decoder and every color plane.
use crate::{Error, Result, bits::Bits, dsp::transform::Dequant, tables};
use std::sync::OnceLock;

#[derive(Clone, Copy, Default)]
struct Entry {
    value: u8,
    bits: u8,
    sub: u16,
}
struct Vlc {
    head: [Entry; 256],
    tail: Vec<[Entry; 256]>,
}
impl Vlc {
    fn new(lengths: &[u8], codes: &[u8]) -> Self {
        let mut table = Self {
            head: [Entry::default(); 256],
            tail: Vec::new(),
        };
        for (value, (&len, &code)) in lengths.iter().zip(codes).enumerate() {
            if len == 0 {
                continue;
            }
            let entry = Entry {
                value: value as u8,
                bits: len,
                sub: 0,
            };
            if len <= 8 {
                let at = (code as usize) << (8 - len);
                table.head[at..at + (1 << (8 - len))].fill(entry);
            } else {
                let first = (code as usize) >> (len - 8);
                if table.head[first].sub == 0 {
                    table.tail.push([Entry::default(); 256]);
                    table.head[first].sub = table.tail.len() as u16;
                }
                let at = ((code as usize) & ((1 << (len - 8)) - 1)) << (16 - len);
                table.tail[table.head[first].sub as usize - 1][at..at + (1 << (16 - len))]
                    .fill(entry);
            }
        }
        table
    }
    #[inline]
    fn read(&self, bits: &mut Bits<'_>) -> Result<usize> {
        let peek = bits.peek_padded(16);
        let mut entry = self.head[(peek >> 8) as usize];
        if entry.sub != 0 {
            entry = self.tail[entry.sub as usize - 1][peek as usize & 255];
        }
        if entry.bits == 0 {
            return Err(if bits.remaining() < 16 {
                Error::Truncated
            } else {
                Error::Invalid(crate::Fault::CavlcPrefix)
            });
        }
        bits.skip(entry.bits as usize)?;
        Ok(entry.value as usize)
    }
}
struct Tables {
    tokens: [Vlc; 4],
    chroma_token: Vlc,
    zeros: [Vlc; 15],
    chroma_zeros: [Vlc; 3],
    run: [Vlc; 7],
}
fn tables() -> &'static Tables {
    static CACHE: OnceLock<Tables> = OnceLock::new();
    CACHE.get_or_init(|| Tables {
        tokens: std::array::from_fn(|i| {
            Vlc::new(
                &tables::COEFF_TOKEN_LEN[i * 68..i * 68 + 68],
                &tables::COEFF_TOKEN_BITS[i * 68..i * 68 + 68],
            )
        }),
        chroma_token: Vlc::new(&tables::CHROMA_TOKEN_LEN, &tables::CHROMA_TOKEN_BITS),
        zeros: std::array::from_fn(|i| {
            Vlc::new(
                &tables::TOTAL_ZEROS_LEN[i * 16..i * 16 + 16],
                &tables::TOTAL_ZEROS_BITS[i * 16..i * 16 + 16],
            )
        }),
        chroma_zeros: std::array::from_fn(|i| {
            Vlc::new(
                &tables::CHROMA_ZEROS_LEN[i * 4..i * 4 + 4],
                &tables::CHROMA_ZEROS_BITS[i * 4..i * 4 + 4],
            )
        }),
        run: std::array::from_fn(|i| {
            Vlc::new(
                &tables::RUN_LEN[i * 16..i * 16 + 16],
                &tables::RUN_BITS[i * 16..i * 16 + 16],
            )
        }),
    })
}

/// nC=-1 is 420 chroma DC. Other blocks (including all 444 planes) use
/// ordinary neighboring nonzero counts. Scan carries transposed DSP positions.
/// Pass None for independently dequantized luma/chroma DC blocks.
pub fn residual(
    bits: &mut Bits<'_>,
    nc: i32,
    scan: &crate::scan::Scan,
    qmul: Option<&[i32]>,
    dst: &mut [i16],
) -> Result<u8> {
    let max = scan.positions().len();
    if !matches!(max, 4 | 15 | 16)
        || (nc < 0 && (nc != -1 || max != 4))
        || nc > 16
        || !scan.fits(dst, qmul)
    {
        return Err(Error::Invalid(crate::Fault::CavlcResidualGeometry));
    }
    let t = tables();
    let token = if nc == -1 {
        t.chroma_token.read(bits)?
    } else {
        t.tokens[if nc < 2 {
            0
        } else if nc < 4 {
            1
        } else if nc < 8 {
            2
        } else {
            3
        }]
        .read(bits)?
    };
    let total = token >> 2;
    let trailing = token & 3;
    if total > max || trailing > total {
        return Err(Error::Invalid(crate::Fault::CavlcCoefficientCount));
    }
    // The private slice driver and inverse transforms own/clear the scratch.
    // AC and the four CAVLC 8x8 scans must not clear one another's coefficients.
    let scan = scan.positions();
    if total == 0 {
        return Ok(0);
    }
    let mut levels = [0i32; 16];
    for level in levels.iter_mut().take(trailing) {
        *level = 1 - 2 * bits.read(1)? as i32;
    }
    let mut suffix = u32::from(total > 10 && trailing < 3);
    for (i, level) in levels.iter_mut().enumerate().take(total).skip(trailing) {
        // One bounded load replaces the unary bit loop. On failure preserve
        // its consumed-zero count (29 rejects overflow, fewer means EOF).
        let prefix = bits.peek_padded(32).leading_zeros();
        if prefix > 28 {
            let consumed = bits.remaining().min(29);
            bits.skip(consumed)?;
            return Err(if consumed < 29 {
                Error::Truncated
            } else {
                Error::Invalid(crate::Fault::CavlcLevelPrefix)
            });
        }
        bits.skip(prefix as usize + 1)?;
        let suffix_bits = if prefix == 14 && suffix == 0 {
            4
        } else if prefix >= 15 {
            prefix - 3
        } else {
            suffix
        };
        let tail = bits.read(suffix_bits)? as i64;
        let mut code = ((prefix.min(15) as i64) << suffix) + tail;
        if prefix >= 15 && suffix == 0 {
            code += 15;
        }
        if prefix >= 16 {
            code += (1i64 << (prefix - 3)) - 4096;
        }
        if i == trailing && trailing < 3 {
            code += 2;
        }
        *level = if code & 1 == 0 {
            ((code + 2) >> 1) as i32
        } else {
            -((code + 1) >> 1) as i32
        };
        if suffix == 0 {
            suffix = 1;
        }
        if suffix < 6 && level.unsigned_abs() > (3 << (suffix - 1)) {
            suffix += 1;
        }
    }
    let mut zeros = if total == max {
        0
    } else if max == 4 {
        t.chroma_zeros[total - 1].read(bits)?
    } else {
        t.zeros[total - 1].read(bits)?
    };
    if zeros + total > max {
        return Err(Error::Invalid(crate::Fault::CavlcTotalZeros));
    }
    let mut position = zeros + total - 1;
    for (i, &level) in levels.iter().enumerate().take(total) {
        let at = scan[position] as usize;
        dst[at] = match qmul {
            Some(q) => Dequant::coefficient(level, q[at]),
            None => i16::try_from(level)
                .map_err(|_| Error::Invalid(crate::Fault::DcCoefficientRange))?,
        };
        if i + 1 < total {
            let run = if zeros == 0 {
                0
            } else {
                t.run[zeros.min(7) - 1].read(bits)?
            };
            if run > zeros || run + 1 > position {
                return Err(Error::Invalid(crate::Fault::CavlcRunBefore));
            }
            zeros -= run;
            position -= run + 1;
        }
    }
    Ok(total as u8)
}
