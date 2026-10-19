// SPDX-License-Identifier: LGPL-2.1-or-later
// Windows SDK dxva.h packed AV1 ABI. Field mapping follows the AV1 spec and
// FFmpeg dxva2_av1.c (license in media/decoder/COPYING.FFmpeg).
use bytemuck::{Pod, Zeroable};
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Reference {
    pub width: u32,
    pub height: u32,
    pub motion: [i32; 6],
    pub flags: u8,
    pub index: u8,
    pub reserved: u16,
}
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Tiles {
    pub cols: u8,
    pub rows: u8,
    pub context: u16,
    pub widths: [u16; 64],
    pub heights: [u16; 64],
}
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct LoopFilter {
    pub levels: [u8; 4],
    pub sharpness: u8,
    pub flags: u8,
    pub refs: [i8; 8],
    pub modes: [i8; 2],
    pub delta_res: u8,
    pub restoration: [u8; 3],
    pub unit: [u16; 3],
    pub reserved: u16,
}
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Quantization {
    pub flags: u8,
    pub base: u8,
    pub deltas: [i8; 5],
    pub matrices: [u8; 3],
    pub reserved: u16,
}
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Cdef {
    pub flags: u8,
    pub y: [u8; 8],
    pub uv: [u8; 8],
}
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Segments {
    pub flags: u8,
    pub reserved: [u8; 3],
    pub masks: [u8; 8],
    pub data: [[i16; 8]; 8],
}
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Grain {
    pub flags: u16,
    pub seed: u16,
    pub y: [[u8; 2]; 14],
    pub num_y: u8,
    pub cb: [[u8; 2]; 10],
    pub num_cb: u8,
    pub cr: [[u8; 2]; 10],
    pub num_cr: u8,
    pub ar_y: [u8; 24],
    pub ar_cb: [u8; 25],
    pub ar_cr: [u8; 25],
    pub cb_mult: u8,
    pub cb_luma: u8,
    pub cr_mult: u8,
    pub cr_luma: u8,
    pub reserved: u8,
    pub cb_offset: i16,
    pub cr_offset: i16,
}
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub current: u8,
    pub superres: u8,
    pub depth: u8,
    pub profile: u8,
    pub tiles: Tiles,
    pub coding: u32,
    pub format: u8,
    pub primary: u8,
    pub order: u8,
    pub order_bits: u8,
    pub refs: [Reference; 7],
    pub map: [u8; 8],
    pub filter: LoopFilter,
    pub quant: Quantization,
    pub cdef: Cdef,
    pub interpolation: u8,
    pub segments: Segments,
    pub grain: Grain,
    pub reserved: u32,
    pub report: u32,
}
#[repr(C, packed)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Tile {
    pub offset: u32,
    pub size: u32,
    pub row: u16,
    pub col: u16,
    pub reserved: u16,
    pub anchor: u8,
    pub reserved8: u8,
}
const _: () = assert!(size_of::<Params>() == 912 && size_of::<Tile>() == 16);
