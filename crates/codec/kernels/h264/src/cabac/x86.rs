// SPDX-License-Identifier: LGPL-2.1-or-later
// Register-resident form of FFmpeg's cabac_functions.h / x86/cabac.h decision
// arithmetic. Original copyright Michael Niedermayer; Rust port OpenUUYC.
use super::{Cabac, Context, tables};

#[inline(always)]
pub(super) fn decision(c: &mut Cabac, context: &mut Context) -> u8 {
    // These fields are private: initialization, normalization and termination
    // preserve 2..510; Context's constructor/transitions preserve 0..127.
    // Therefore every readonly table access below is within CABAC[0..1343].
    debug_assert!((2..=510).contains(&c.range));
    debug_assert!(context.0 <= 127);
    let mut low = c.low;
    let mut range = c.range;
    let mut state = context.0 as u32;
    let bit: u32;
    unsafe {
        core::arch::asm!(
            "mov {state:e}, {state:e}",
            "mov ecx, {range:e}",
            "and ecx, 192",
            "lea ecx, [rcx*2 + {state:r}]",
            "movzx {bit:e}, byte ptr [{table} + rcx + 512]",
            "sub {range:e}, {bit:e}",
            "mov ecx, {range:e}",
            "shl ecx, 17",
            "cmp {low:e}, ecx",
            "cmovae {range:e}, {bit:e}",
            "mov {bit:e}, 0",
            "setae {bit:l}",
            "neg {bit:e}",
            "and ecx, {bit:e}",
            "sub {low:e}, ecx",
            "xor {state:e}, {bit:e}",
            "mov {bit:e}, {state:e}",
            "and {bit:e}, 1",
            "add {state:e}, 128",
            "movzx {state:e}, byte ptr [{table} + {state:r} + 1024]",
            "movzx ecx, byte ptr [{table} + {range:r}]",
            "shl {range:e}, cl",
            "shl {low:e}, cl",
            table=in(reg) tables::CABAC.as_ptr(),
            low=inout(reg) low,
            range=inout(reg) range,
            state=inout(reg) state,
            bit=out(reg) bit,
            out("ecx") _,
            options(nostack,readonly),
        );
    }
    c.low = low;
    c.range = range;
    context.0 = state as u8;
    bit as u8
}
