# Linux 平台探针

这些小程序不是产品代码，也不参与构建。它们用来在真实驱动上**验证假设**，
在写 `src/platform/linux/dmabuf.rs` 之前确认零拷贝方案到底可不可行。

保留它们的原因是：零拷贝这条路径的可行性完全取决于驱动行为，而这些行为
没有文档、各家实现不同。以后换驱动、换 GPU，或者怀疑某台机器上 dmabuf 路径
为何回退到 CPU 时，用同样的探针先测一遍，比在应用里加日志再猜要快得多。

## vaapi_export_probe.c

验证 VA-API 能否把 NV12 解码表面导出为 DRM PRIME dmabuf，并打印实际布局
（fourcc、object 数、平面偏移与 pitch、DRM modifier）。

```sh
gcc -O2 -o vaapi_export_probe vaapi_export_probe.c -lva -lva-drm
./vaapi_export_probe /dev/dri/renderD128 1920 1080
```

它回答的是「第一道门槛」：驱动愿不愿意导出。注意它导出的是一张没解码过的空表面：nvidia-vaapi-driver 要等表面解码过一帧才分配显存，所以在 NVIDIA 上会报 `invalid VASurfaceID`，这不代表 NVIDIA 不能导出，只是本探针在 NVIDIA 上测不出结论。零拷贝要求单 buffer、双平面的
NV12 布局；输出里 `num_objects=1` 且 `num_planes=2` 才满足，否则
`export_nv12` 会回退到 CPU 读回。

## vulkan_dmabuf_probe.c

在第一道门槛通过后，把同一个 dmabuf 用 Vulkan 导入为两个单平面 image
（Y 用 `R8Unorm`，UV 用 `R8G8Unorm`），流程与 wgpu-hal 的
`texture_from_dmabuf_fd` 一致：

```
ExternalMemoryImageCreateInfo(DMA_BUF)
+ ImageDrmFormatModifierExplicitCreateInfoEXT
+ Tiling::DRM_FORMAT_MODIFIER
+ ImportMemoryFdInfoKHR
```

```sh
gcc -O2 -o vulkan_dmabuf_probe vulkan_dmabuf_probe.c -lva -lva-drm -lvulkan
./vulkan_dmabuf_probe /dev/dri/renderD128 1920 1080
```

它回答的是「第二道门槛」：驱动导出的 DRM modifier，Vulkan 侧认不认。
两道都过，并设 `OPENUUYC_ZEROCOPY=1`（默认关闭）后，才会真正走上零拷贝。

## gpu-conversion

Standalone hardware test for the independent EGL/GLES RGB DMA-BUF → owned
NV12 DMA-BUF → VA-API H.264 path. It compares software conversion and direct
GPU input at the same size, validates color bars and a per-pixel gradient with
a cropped origin, overwrites the source before importing the result on a
worker thread, exercises CPU readback fallback, and sends P frames and periodic
IDRs. The probe includes the production converter and VA encoder source files;
it does not participate in the app build.

On Linux with libva, EGL/GLES3 and a Haswell-or-newer render node:

```sh
cargo run --release --manifest-path devtools/probes/gpu-conversion/Cargo.toml -- 2880 1800 64
ffmpeg -v error -i /tmp/uu-gpu-conversion-2880x1800.h264 -f null -
```

The product path can be exercised for comparison with
`OPENUUYC_GPU_CONVERT=1`. It remains opt-in while its end-to-end throughput is
measured on the target system.

### Additional binaries

The same crate builds focused probes used to validate the conversion and its
interaction with the encoder. All of them link the production
`gpu_convert.rs` and `vaapi_encoder.rs`:

- `overlap` — the decisive throughput test. It runs the real converter and
  encoder on **two threads** (producer converts, consumer encodes) and compares
  the CPU path, the GPU path and a serial single-thread GPU path. Measuring
  conversion and encode serially is misleading: the converter's per-frame
  `glFinish` is latency that the encoder's own runtime absorbs when the two run
  on separate threads, which is how production splits them.
- `sync` — converts, immediately repaints the source with a different colour,
  then imports the owned NV12 on the encoder's own display and checks every
  plane. Proves the snapshot is stable without an extra copy.
- `churn` — long-running stress over fresh source surfaces and exports, one or
  two threads, to catch leaks or lifetime races in the EGL/GL/VA objects.
- `cycle` — creates and drops `Converter` repeatedly, sequentially and on two
  concurrent threads, to exercise the `eglTerminate`/`gbm_device_destroy` path.
- `scale` — verifies the GPU scaler plane by plane, on the whole frame and on
  an odd crop origin. At equal sizes every Y and UV byte must be within the
  final rounding of the BT.601 limited-range value of its source texels (the
  exact `texelFetch` pass); when scaling, every byte must stay within 2 of a
  bilinear reference sampled at the pixel centres the CPU `scale` uses, and the
  same reference moved by half a source pixel must fail that bound, so a
  shifted sampling grid cannot pass. Repeated conversions must be identical.
- `mismatch` — measures the cost of a controller quality whose output geometry
  differs from the source. It times three passes: `native` (no mismatch),
  `readback` (the CPU fallback the scaler replaced: convert at the source size,
  `OwnedNv12::pixels()`, the CPU `scale`, encode the BGRA) and `gpu-scale`
  (convert straight to the target). Measured on Haswell from 2880x1800 to
  1280x720 and 2560x1440: readback 89.5 and 172 ms/frame, GPU scale 8.8 and
  13.9 ms/frame, both below the native pass.

```sh
cargo run --release --manifest-path devtools/probes/gpu-conversion/Cargo.toml --bin overlap -- 2880 1800 128
cargo run --release --manifest-path devtools/probes/gpu-conversion/Cargo.toml --bin churn -- 2880 1800 1500 2
cargo run --release --manifest-path devtools/probes/gpu-conversion/Cargo.toml --bin scale -- 2880 1800 1728 1080
cargo run --release --manifest-path devtools/probes/gpu-conversion/Cargo.toml --bin mismatch -- 2880 1800 1728 1080 96
```

Measured on Haswell at idle (load < 2), 2880x1800: pipelined CPU 40-41 fps vs
pipelined GPU 60-63 fps; 1920x1080: 82 fps vs 125-132 fps. The serial GPU
number is *slower* than either, which is the trap the two-thread model avoids.

## 已知实例

Haswell（Iris Pro P5200，i965 驱动）+ HasVK，1920x1080：

```
modifier=0x100000000000002  y_pitch=1920  uv_pitch=1920  uv_offset=2088960
[Y]  导入成功 format=9  1920x1080  offset=0       stride=1920
[UV] 导入成功 format=16 960x540    offset=2088960 stride=1920
```

两道门槛都过，所以该机器上零拷贝生效：有会话 CPU 从约 50% 降到 13%，
RSS 从 341 MB 降到 234 MB。
