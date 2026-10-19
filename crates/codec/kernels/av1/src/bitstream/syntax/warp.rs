// SPDX-License-Identifier: BSD-2-Clause
// Copyright (c) 2018, VideoLAN and dav1d authors
// Copyright (c) 2018, Two Orioles, LLC
// Global-motion shear validity from dav1d warpmv.c; ../../COPYING.dav1d.
fn round_signed(value: i64, shift: u32) -> i64 {
    if shift == 0 {
        return value;
    }
    let magnitude = (value.unsigned_abs() + (1u64 << (shift - 1))) >> shift;
    if value < 0 {
        -(magnitude as i64)
    } else {
        magnitude as i64
    }
}
fn reduce(value: i64) -> i32 {
    let v = value.clamp(-32768, 32767);
    (round_signed(v, 6) * 64) as i32
}

/// Invalid shears disable warped prediction, as specified by AV1. Callers
/// retain the corresponding translational motion-vector prediction.
pub(super) fn valid(matrix: [i32; 6]) -> Option<()> {
    if matrix[2] <= 0 {
        return None;
    }
    if matrix[2..] == [65536, 0, 0, 65536] {
        return Some(());
    }
    let alpha = reduce(i64::from(matrix[2]) - 65536);
    let beta = reduce(i64::from(matrix[3]));
    if 4 * alpha.abs() + 7 * beta.abs() >= 65536 {
        return None;
    }
    let d = matrix[2] as u32;
    let log = d.ilog2();
    let e = d - (1 << log);
    let index = if log > 8 {
        (e + (1 << (log - 9))) >> (log - 8)
    } else {
        e << (8 - log)
    };
    let factor = i64::from(DIVISOR[index as usize]);
    let shift = log + 14;
    let v1 = (i64::from(matrix[4]) * 65536).checked_mul(factor)?;
    let gamma = reduce(round_signed(v1, shift));
    let v2 = (i64::from(matrix[3]) * i64::from(matrix[4])).checked_mul(factor)?;
    let delta = reduce(i64::from(matrix[5]) - round_signed(v2, shift) - 65536);
    if 4 * gamma.abs() + 4 * delta.abs() >= 65536 {
        return None;
    }
    Some(())
}

const DIVISOR: [u16; 257] = [
    16384, 16320, 16257, 16194, 16132, 16070, 16009, 15948, 15888, 15828, 15768, 15709, 15650,
    15592, 15534, 15477, 15420, 15364, 15308, 15252, 15197, 15142, 15087, 15033, 14980, 14926,
    14873, 14821, 14769, 14717, 14665, 14614, 14564, 14513, 14463, 14413, 14364, 14315, 14266,
    14218, 14170, 14122, 14075, 14028, 13981, 13935, 13888, 13843, 13797, 13752, 13707, 13662,
    13618, 13574, 13530, 13487, 13443, 13400, 13358, 13315, 13273, 13231, 13190, 13148, 13107,
    13066, 13026, 12985, 12945, 12906, 12866, 12827, 12788, 12749, 12710, 12672, 12633, 12596,
    12558, 12520, 12483, 12446, 12409, 12373, 12336, 12300, 12264, 12228, 12193, 12157, 12122,
    12087, 12053, 12018, 11984, 11950, 11916, 11882, 11848, 11815, 11782, 11749, 11716, 11683,
    11651, 11619, 11586, 11555, 11523, 11491, 11460, 11429, 11398, 11367, 11336, 11305, 11275,
    11245, 11215, 11185, 11155, 11125, 11096, 11067, 11038, 11009, 10980, 10951, 10923, 10894,
    10866, 10838, 10810, 10782, 10755, 10727, 10700, 10673, 10645, 10618, 10592, 10565, 10538,
    10512, 10486, 10460, 10434, 10408, 10382, 10356, 10331, 10305, 10280, 10255, 10230, 10205,
    10180, 10156, 10131, 10107, 10082, 10058, 10034, 10010, 9986, 9963, 9939, 9916, 9892, 9869,
    9846, 9823, 9800, 9777, 9754, 9732, 9709, 9687, 9664, 9642, 9620, 9598, 9576, 9554, 9533, 9511,
    9489, 9468, 9447, 9425, 9404, 9383, 9362, 9341, 9321, 9300, 9279, 9259, 9239, 9218, 9198, 9178,
    9158, 9138, 9118, 9098, 9079, 9059, 9039, 9020, 9001, 8981, 8962, 8943, 8924, 8905, 8886, 8867,
    8849, 8830, 8812, 8793, 8775, 8756, 8738, 8720, 8702, 8684, 8666, 8648, 8630, 8613, 8595, 8577,
    8560, 8542, 8525, 8508, 8490, 8473, 8456, 8439, 8422, 8405, 8389, 8372, 8355, 8339, 8322, 8306,
    8289, 8273, 8257, 8240, 8224, 8208, 8192,
];
