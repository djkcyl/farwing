use super::{Encoder, Runtime};
use crate::platform::windows::capture::{EncodingAdapter, create_device};
use crate::platform::windows::format::{
    Backend, Capability, Codec, Color, Format, QualityTarget, Rate,
};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*};

const SAMPLE_SIZE: (u32, u32) = (1280, 720);

#[derive(Default)]
pub(crate) struct ProbeCache {
    hardware: HashMap<u64, AdapterProbes>,
    software: Option<Capability>,
}

struct AdapterProbes {
    adapter: EncodingAdapter,
    // Failed probes are observations for this inventory, not permanent exclusions.
    formats: HashMap<Format, Option<Capability>>,
}

fn formats(backend: Backend) -> impl Iterator<Item = Format> {
    [Codec::H264, Codec::H265, Codec::Av1]
        .into_iter()
        .flat_map(|codec| {
            [1, 3].into_iter().flat_map(move |chroma| {
                [8, 10].into_iter().map(move |depth| Format {
                    codec,
                    chroma,
                    depth,
                })
            })
        })
        .filter(move |&format| backend.accepts(format))
}

impl ProbeCache {
    pub(crate) fn prepare(
        &mut self,
        source_adapter: u64,
        render_adapter: u64,
        adapters: &[EncodingAdapter],
        gpu_compatibility: bool,
        is_active: impl Fn() -> bool,
    ) -> Result<Vec<Capability>> {
        self.hardware
            .retain(|_, cached| adapters.contains(&cached.adapter));
        let mut sorted = adapters.iter().collect::<Vec<_>>();
        sorted.sort_by_key(|adapter| adapter.luid != render_adapter);
        let mut runtime = None;
        let mut capabilities = Vec::new();
        for adapter in sorted {
            ensure!(is_active(), "被控准备已取消");
            let backend = match adapter.vendor {
                0x10de => Backend::Nvidia,
                0x1002 => Backend::Amd,
                0x8086 => Backend::Intel,
                _ => continue,
            };
            let tested = &mut self
                .hardware
                .entry(adapter.luid)
                .or_insert_with(|| AdapterProbes {
                    adapter: adapter.clone(),
                    formats: HashMap::new(),
                })
                .formats;
            let mut device = None;
            let mut samples = [None, None];
            for format in formats(backend) {
                ensure!(is_active(), "被控准备已取消");
                if let Some(cached) = tested.get(&format) {
                    capabilities.extend(cached.iter().cloned());
                    continue;
                }
                if device.is_none() {
                    if runtime.is_none() {
                        runtime = Some(Runtime::with_gpu_compatibility(gpu_compatibility)?);
                    }
                    match create_device(adapter.luid) {
                        Ok((created, _)) => device = Some(created),
                        Err(error) => {
                            tracing::debug!(%error, adapter = adapter.luid, "encoder adapter unavailable");
                            break;
                        }
                    }
                }
                let device = device.as_ref().unwrap();
                let hdr = format.depth == 10;
                let sample =
                    samples[usize::from(hdr)].get_or_insert_with(|| sample_texture(device, hdr));
                let sample = match sample {
                    Ok(sample) => sample,
                    Err(error) => {
                        tracing::debug!(%error, ?format, adapter = adapter.luid, "encoder probe texture unavailable");
                        continue;
                    }
                };
                let capability = verify(device, sample, adapter.luid, backend, format, &is_active);
                if let Err(error) = unsafe { device.GetDeviceRemovedReason() } {
                    tested.clear();
                    capabilities.retain(|capability| capability.adapter != adapter.luid);
                    tracing::warn!(%error, adapter = adapter.luid, "encoder probe device lost");
                    break;
                }
                // Keep verified results if a later format is cancelled with the session.
                if capability.is_none() {
                    ensure!(is_active(), "被控准备已取消");
                }
                capabilities.extend(capability.iter().cloned());
                tested.insert(format, capability);
            }
        }
        ensure!(is_active(), "被控准备已取消");
        if self.software.is_none() {
            if runtime.is_none() {
                runtime = Some(Runtime::with_gpu_compatibility(gpu_compatibility)?);
            }
            let software = (|| {
                let (device, _) = create_device(source_adapter)?;
                let sample = sample_texture(&device, false)?;
                Ok::<_, anyhow::Error>(verify(
                    &device,
                    &sample,
                    source_adapter,
                    Backend::Software,
                    Format::AVC,
                    &is_active,
                ))
            })();
            ensure!(is_active(), "被控准备已取消");
            match software {
                Ok(capability) => self.software = capability,
                Err(error) => tracing::debug!(%error, "software encoder probe input unavailable"),
            }
        }
        if let Some(capability) = &self.software {
            capabilities.push(Capability {
                adapter: source_adapter,
                ..capability.clone()
            });
        }
        ensure!(!capabilities.is_empty(), "没有可用的视频编码器");
        tracing::info!(
            probed = runtime.is_some(),
            capabilities = capabilities.len(),
            "host encoder capabilities ready"
        );
        Ok(capabilities)
    }
}

// Use the encoder's own device and the same RGB input formats as live capture.
fn sample_texture(device: &ID3D11Device, hdr: bool) -> Result<ID3D11Texture2D> {
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: SAMPLE_SIZE.0,
                Height: SAMPLE_SIZE.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: if hdr {
                    DXGI_FORMAT_R16G16B16A16_FLOAT
                } else {
                    DXGI_FORMAT_B8G8R8A8_UNORM
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            },
            None,
            Some(&mut texture),
        )?;
        let texture = texture.context("创建编码探测纹理")?;
        let mut target = None;
        device.CreateRenderTargetView(&texture, None, Some(&mut target))?;
        device.GetImmediateContext()?.ClearRenderTargetView(
            &target.context("创建编码探测视图")?,
            &[0.25, 0.5, 0.75, 1.0],
        );
        Ok(texture)
    }
}

fn verify(
    device: &ID3D11Device,
    sample: &ID3D11Texture2D,
    adapter: u64,
    backend: Backend,
    format: Format,
    is_active: impl Fn() -> bool,
) -> Option<Capability> {
    let result = (|| -> Result<Capability> {
        let rate = Rate {
            target: 2_000_000,
            peak: 2_000_000,
            fps: 30,
            quality: 1,
            quality_target: QualityTarget {
                bitrate: 2_000_000,
                fps: 30,
            },
        };
        let mut encoder = if backend == Backend::Software {
            Encoder::software_format(device, SAMPLE_SIZE, format, rate)?
        } else {
            Encoder::hardware_format(
                device,
                SAMPLE_SIZE,
                format,
                rate,
                if format.depth == 10 {
                    Color::Hdr
                } else {
                    Color::Sdr
                },
            )?
        };
        for i in 0..20 {
            ensure!(is_active(), "被控准备已取消");
            let frames = encoder.encode(sample, i * 333333, true)?;
            if let Some(output) = frames.iter().find_map(|frame| {
                crate::media::video_format::parse_stream_format(format.codec.media(), &frame.data)
            }) {
                ensure!(
                    output.chroma_format_idc == format.chroma
                        && output.bit_depth_luma == format.depth
                        && output.bit_depth_chroma == format.depth
                        && (output.visible_width, output.visible_height) == SAMPLE_SIZE,
                    "编码器实际码流与请求格式不一致"
                );
                return Ok(Capability {
                    adapter,
                    backend,
                    format,
                    maximum: encoder.maximum_size(),
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        anyhow::bail!("编码器未输出可验证的序列头")
    })();
    match result {
        Ok(capability) => {
            tracing::debug!(
                ?capability,
                "host encoder capability verified from sequence header"
            );
            Some(capability)
        }
        Err(error) => {
            tracing::debug!(%error, adapter, ?backend, ?format, "host encoder capability rejected");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_encoder_capabilities_follow_adapter_instances() {
        let gpu = |luid| EncodingAdapter {
            id: None,
            luid,
            vendor: 0x10de,
            name: "identical GPU model".into(),
        };
        let first = gpu(u64::MAX);
        let second = gpu(u64::MAX - 1);
        let unavailable = gpu(u64::MAX - 2);
        let mut cache = ProbeCache::default();
        for (adapter, maximum) in [(&first, (4096, 4096)), (&second, (8192, 8192))] {
            let formats = formats(Backend::Nvidia)
                .map(|format| {
                    let result = (format == Format::AVC).then_some(Capability {
                        adapter: adapter.luid,
                        backend: Backend::Nvidia,
                        format,
                        maximum,
                    });
                    (format, result)
                })
                .collect();
            cache.hardware.insert(
                adapter.luid,
                AdapterProbes {
                    adapter: adapter.clone(),
                    formats,
                },
            );
        }
        cache.hardware.insert(
            unavailable.luid,
            AdapterProbes {
                adapter: unavailable.clone(),
                formats: formats(Backend::Nvidia)
                    .map(|format| (format, None))
                    .collect(),
            },
        );
        cache.software = Some(Capability {
            adapter: first.luid,
            backend: Backend::Software,
            format: Format::AVC,
            maximum: (1920, 1080),
        });

        let reused = cache
            .prepare(
                first.luid,
                second.luid,
                &[first.clone(), second.clone(), unavailable.clone()],
                false,
                || true,
            )
            .unwrap();
        let hardware = reused
            .iter()
            .filter(|cap| cap.backend != Backend::Software)
            .map(|cap| (cap.adapter, cap.maximum))
            .collect::<Vec<_>>();
        assert_eq!(
            hardware,
            [(second.luid, (8192, 8192)), (first.luid, (4096, 4096))]
        );
        assert!(
            reused
                .iter()
                .any(|cap| cap.backend == Backend::Software && cap.adapter == first.luid)
        );

        assert!(
            cache.hardware[&unavailable.luid]
                .formats
                .contains_key(&Format::AVC)
        );
        let remaining = cache
            .prepare(first.luid, first.luid, &[first], false, || true)
            .unwrap();
        assert!(!remaining.iter().any(|cap| cap.adapter == second.luid));
        assert!(!cache.hardware.contains_key(&second.luid));
    }
}
