use anyhow::{Context, Result};
use windows::Win32::Graphics::{
    Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Direct3D11::*, Dxgi::Common::*,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub quality: i32,
    pub maximum: (u32, u32),
    pub hdr: bool,
}

pub(crate) struct Converter {
    timing: Option<super::gpu_diagnostics::Timing>,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    params: ID3D11Buffer,
    input: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    output: Option<(ID3D11Texture2D, ID3D11RenderTargetView)>,
    source: (u32, u32, DXGI_FORMAT),
    destination: (u32, u32, bool),
}
impl Converter {
    pub(crate) fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            let (mut vertex, mut pixel, mut sampler, mut params) = (None, None, None, None);
            device.CreateVertexShader(include_bytes!("capture_vs.cso"), None, Some(&mut vertex))?;
            device.CreatePixelShader(include_bytes!("capture_ps.cso"), None, Some(&mut pixel))?;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: 48,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut params),
            )?;
            Ok(Self {
                timing: super::gpu_diagnostics::Timing::new(device, "capture preprocess"),
                vertex: vertex.context("采集vertex shader")?,
                pixel: pixel.context("采集pixel shader")?,
                sampler: sampler.context("采集sampler")?,
                params: params.context("采集参数buffer")?,
                input: None,
                output: None,
                source: (0, 0, DXGI_FORMAT_UNKNOWN),
                destination: (0, 0, false),
            })
        }
    }
    pub(crate) fn convert(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        desc: D3D11_TEXTURE2D_DESC,
        rotation: DXGI_MODE_ROTATION,
        white: f32,
        source_hdr: Option<crate::media::video_color::HdrMetadata>,
        request: Request,
        pointer: Option<(ID3D11ShaderResourceView, [f32; 8])>,
    ) -> Result<ID3D11Texture2D> {
        let _span = self.timing.as_mut().and_then(|timer| timer.begin());
        unsafe {
            if self.source != (desc.Width, desc.Height, desc.Format) {
                let mut copy = None;
                let mut view = None;
                device.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        Usage: D3D11_USAGE_DEFAULT,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                        ..desc
                    },
                    None,
                    Some(&mut copy),
                )?;
                let copy = copy.context("采集shader纹理")?;
                device.CreateShaderResourceView(&copy, None, Some(&mut view))?;
                self.input = Some((copy, view.context("采集shader视图")?));
                self.source = (desc.Width, desc.Height, desc.Format);
            }
            let rotated = if matches!(
                rotation,
                DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270
            ) {
                (desc.Height, desc.Width)
            } else {
                (desc.Width, desc.Height)
            };
            let size = super::output_size(rotated.0, rotated.1, request.quality);
            let size = super::fit_size(size.0, size.1, request.maximum);
            let hdr = request.hdr;
            let input = self.input.as_ref().context("采集转换输入缺失")?;
            if input.0 != *texture {
                context.CopyResource(&input.0, texture);
            }
            // Encode consumes RGB only (alpha is discarded by every backend).
            // Keep our owned snapshot, never return the duplication resource.
            if size == (desc.Width, desc.Height)
                && matches!(
                    rotation,
                    DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED
                )
                && !hdr
                && desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM
                && pointer.is_none()
            {
                return Ok(input.0.clone());
            }
            if self.destination != (size.0, size.1, hdr) {
                let mut output = None;
                let mut target = None;
                device.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: size.0,
                        Height: size.1,
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
                        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0)
                            as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut output),
                )?;
                let output = output.context("采集转换输出")?;
                device.CreateRenderTargetView(&output, None, Some(&mut target))?;
                self.output = Some((output, target.context("采集转换目标")?));
                self.destination = (size.0, size.1, hdr);
            }
            let input = self.input.as_ref().context("采集转换输入缺失")?;
            let output = self.output.as_ref().context("采集转换输出缺失")?;
            let mut values = [0f32; 12];
            values[..4].copy_from_slice(&[
                rotation.0 as f32,
                if hdr {
                    if desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                        4.0
                    } else {
                        3.0
                    }
                } else if desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                    if source_hdr.is_some() { 2.0 } else { 1.0 }
                } else {
                    0.0
                },
                white,
                source_hdr.map_or(1000., |m| f32::from(m.max_luminance)),
            ]);
            let pointer_view = pointer.map(|(view, metadata)| {
                values[4..].copy_from_slice(&metadata);
                view
            });
            context.UpdateSubresource(&self.params, 0, None, values.as_ptr().cast(), 0, 0);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.IASetInputLayout(None);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.pixel, None);
            context.PSSetShaderResources(0, Some(&[Some(input.1.clone()), pointer_view]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: size.0 as f32,
                Height: size.1 as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            context.OMSetRenderTargets(Some(&[Some(output.1.clone())]), None);
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&[None, None]));
            context.OMSetRenderTargets(None, None);
            Ok(output.0.clone())
        }
    }
}
