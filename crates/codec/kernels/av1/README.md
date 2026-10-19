# Farwing AV1 kernel

One Rust crate for AV1 software decoding and bitstream syntax:

- `src/bitstream`: OBU metadata, sequence headers and hardware decode syntax.
- `src/decoder`: x86/x86_64 software decoder and owned-pixel packing for
  4:2:0 / 4:4:4, 8 / 10-bit output.
- `build.rs`: decoder SIMD assembly; Cargo tracks included assembly files.
  Disabling `asm` omits NASM/CC build dependencies.

The decoder retains x86/x86_64 SIMD and scalar kernels. Other CPU architectures
use the bitstream module without a software decoder. The application accesses
these modules through `farwing-codec`.

Decoding derives from rav1d revision
`d3d1cd67059f47803919be8276650e5870c9fd02`. Bitstream parsing includes Chromium
and dav1d-derived code. Retained decoder assembly includes contributions from
rav1e authors. Original source notices, licenses and patent grants are retained
under `licenses/`.

Software decoding uses bounded frame delay, cooperative cancellation and
reference reset. Film grain remains outside the supported streaming subset.
