# Farwing video codecs

One library for low-delay desktop streaming. AV1 software decoding
is available only on x86 and x86_64; other architectures report AV1 software
support as unavailable. Shared bitstream parsing remains architecture-independent.

| Format | Software encoding | Software decoding |
| --- | --- | --- |
| H.264 | 4:2:0, 8-bit | 4:2:0 / 4:4:4, 8-bit |
| AV1 | — | 4:2:0 / 4:4:4, 8 / 10-bit |
| H.265 | — | Syntax for the application's hardware decoder |

- `encoder`: reusable input planes, immediate output, live bitrate/FPS updates,
  keyframe requests and cancellation recovery. After `prepare` returns, the caller
  can unmap its input before `encode` starts; no future frames are queued.
- `decoder`: owned pixel output, frame tokens, cancellation and reference reset.
- `Format`: common software capability checks.
- `syntax`: format-specific bitstream parsers shared with hardware adapters.
- `kernels`: one internal crate per codec (H.264, AV1, HEVC syntax). AV1 parsing and
  decoding are modules in the same crate. Each codec retains its
  SIMD and optimization settings; the application depends only on `farwing-codec`.

GPU resources, capture, transport and UI belong to the application. Software
encoding supports H.264 up to 3840×2160 / 144 FPS; these are negotiated upper
bounds, not a guarantee of real-time throughput. Software decoding keeps its
existing resolution support. The application still allows one software
playback window at a time.

CPU dispatch keeps a portable path and selects supported SIMD at runtime.
AV1 decoding uses up to four workers with a one-frame delay limit.

The library includes code derived from FFmpeg, OpenH264, OxideAV, Chromium,
rav1e and rav1d. Keep the notices alongside each kernel; see
[THIRD_PARTY_NOTICES](../../THIRD_PARTY_NOTICES) for attribution and licenses.

```powershell
cargo test --manifest-path crates/codec/Cargo.toml -p farwing-h264 --lib
```
