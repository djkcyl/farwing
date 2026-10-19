# H.264 core

Rust H.264 decoding and low-delay desktop encoding used by Farwing.

- Decoder: progressive 8-bit I/P pictures, CAVLC/CABAC, 4:2:0 and 4:4:4.
- Encoder: low-delay desktop video with bitrate/frame-rate updates and keyframe requests.
- Portable DSP with runtime-selected SSE2, SSSE3 and AVX2 implementations.

```powershell
cargo test --manifest-path crates/codec/Cargo.toml -p farwing-h264 --lib
```

AVC syntax and parameter tracking are shared by software and DXVA decoding.
Selected OxideAV-derived primitives retain [their MIT notice](COPYING.OxideAV);
there is no oxideav runtime dependency.

The crate does not link FFmpeg or OpenH264. FFmpeg-derived code retains
LGPL-2.1-or-later and original author notices; see [COPYING.LGPLv2.1](COPYING.LGPLv2.1).
OpenH264-derived encoder algorithms retain the
[BSD notice](../../../../licenses/openh264-algorithms.txt).
Component provenance is recorded in [THIRD_PARTY_NOTICES](../../../../THIRD_PARTY_NOTICES).
