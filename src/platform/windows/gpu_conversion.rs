//! Shared D3D11 input contract for NVENC, AMF and QSV. No raw CPU readback.
use super::format::{Color, Format};
use anyhow::{Context, Result, ensure};
use windows::Win32::Graphics::{
    Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, Direct3D11::*, Dxgi::Common::*,
};

pub(crate) struct Conversion {
    timing: Option<super::gpu_diagnostics::Timing>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    engine: Engine,
    pub output: ID3D11Texture2D,
    pub color: Color,
    size: (u32, u32),
    inputs: Vec<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
}
enum Engine {
    Compute {
        shader: ID3D11ComputeShader,
        views: Vec<Option<ID3D11UnorderedAccessView>>,
    },
    Pixel {
        vertex: ID3D11VertexShader,
        planes: Vec<(ID3D11PixelShader, ID3D11RenderTargetView)>,
    },
}
impl Conversion {
    pub fn is_compute(&self) -> bool {
        matches!(self.engine, Engine::Compute { .. })
    }
    pub fn bind_flags(&self) -> u32 {
        match self.engine {
            Engine::Compute { .. } => D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            Engine::Pixel { .. } => D3D11_BIND_RENDER_TARGET.0 as u32,
        }
    }
    pub fn new(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        color: Color,
    ) -> Result<Self> {
        Self::aligned(device, size, format, 1, color)
    }
    pub fn aligned(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        alignment: u32,
        color: Color,
    ) -> Result<Self> {
        Self::with_path(
            device,
            size,
            format,
            alignment,
            color,
            format.chroma == 1,
            true,
        )
    }
    pub fn compute(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        alignment: u32,
        color: Color,
    ) -> Result<Self> {
        ensure!(format.chroma == 1, "该格式仅使用pixel输入候选");
        Self::with_path(device, size, format, alignment, color, true, false)
    }
    pub fn pixel(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        alignment: u32,
        color: Color,
    ) -> Result<Self> {
        Self::with_path(device, size, format, alignment, color, false, true)
    }
    fn with_path(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        alignment: u32,
        color: Color,
        prefer_compute: bool,
        fallback: bool,
    ) -> Result<Self> {
        ensure!(
            format.valid()
                && (!color.is_hdr() || format.depth == 10)
                && size.0 > 0
                && size.1 > 0
                && size.0 % 2 == 0
                && size.1 % 2 == 0,
            "无效编码纹理参数"
        );
        let (storage, planes, shader): (_, Vec<_>, &[u8]) = match (format.chroma, format.depth) {
            (1, 8) => (
                DXGI_FORMAT_NV12,
                vec![DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM],
                include_bytes!("encode_nv12.cso"),
            ),
            (1, 10) => (
                DXGI_FORMAT_P010,
                vec![DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM],
                if color.is_hdr() {
                    include_bytes!("encode_p010.cso")
                } else {
                    include_bytes!("encode_p010_sdr.cso")
                },
            ),
            (3, 8) => (
                DXGI_FORMAT_AYUV,
                vec![DXGI_FORMAT_R8G8B8A8_UNORM],
                include_bytes!("encode_ayuv.cso"),
            ),
            (3, 10) => (
                DXGI_FORMAT_Y410,
                vec![DXGI_FORMAT_R10G10B10A2_UNORM],
                if color.is_hdr() {
                    include_bytes!("encode_y410.cso")
                } else {
                    include_bytes!("encode_y410_sdr.cso")
                },
            ),
            _ => unreachable!(),
        };
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: size.0.div_ceil(alignment) * alignment,
                Height: size.1.div_ceil(alignment) * alignment,
                MipLevels: 1,
                ArraySize: 1,
                Format: storage,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_UNORDERED_ACCESS.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            let compute = (|| -> Result<(ID3D11Texture2D, Engine)> {
                ensure!(prefer_compute, "pixel conversion selected");
                let mut output = None;
                device.CreateTexture2D(&desc, None, Some(&mut output))?;
                let output = output.context("创建硬件编码输入纹理")?;
                let mut views = Vec::new();
                for &plane in &planes {
                    let mut view = None;
                    device.CreateUnorderedAccessView(
                        &output,
                        Some(&D3D11_UNORDERED_ACCESS_VIEW_DESC {
                            Format: plane,
                            ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
                            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                                Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
                            },
                        }),
                        Some(&mut view),
                    )?;
                    views.push(Some(view.context("创建编码输入平面视图")?));
                }
                let mut compute = None;
                device.CreateComputeShader(shader, None, Some(&mut compute))?;
                Ok((
                    output,
                    Engine::Compute {
                        shader: compute.context("创建编码色彩转换器")?,
                        views,
                    },
                ))
            })();
            let (output, engine) = match compute {
                Ok(compute) => compute,
                Err(error) => {
                    if !fallback {
                        return Err(error);
                    }
                    if prefer_compute {
                        tracing::debug!(%error,?format,"GPU compute color path unavailable; creating pixel path");
                    }
                    let mut output = None;
                    device.CreateTexture2D(
                        &D3D11_TEXTURE2D_DESC {
                            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0)
                                as u32,
                            ..desc
                        },
                        None,
                        Some(&mut output),
                    )?;
                    let output = output.context("创建像素转换输出纹理")?;
                    let shaders: Vec<&[u8]> = match (format.chroma, format.depth) {
                        (1, 8) => vec![
                            include_bytes!("encode_nv12_y_ps.cso"),
                            include_bytes!("encode_nv12_uv_ps.cso"),
                        ],
                        (1, 10) => vec![
                            if color.is_hdr() {
                                include_bytes!("encode_p010_y_ps.cso")
                            } else {
                                include_bytes!("encode_p010_sdr_y_ps.cso")
                            },
                            if color.is_hdr() {
                                include_bytes!("encode_p010_uv_ps.cso")
                            } else {
                                include_bytes!("encode_p010_sdr_uv_ps.cso")
                            },
                        ],
                        (3, 8) => vec![include_bytes!("encode_ayuv_ps.cso")],
                        (3, 10) => vec![if color.is_hdr() {
                            include_bytes!("encode_y410_ps.cso")
                        } else {
                            include_bytes!("encode_y410_sdr_ps.cso")
                        }],
                        _ => unreachable!(),
                    };
                    let mut targets = Vec::new();
                    for (plane, bytes) in planes.into_iter().zip(shaders) {
                        let (mut shader, mut target) = (None, None);
                        device.CreatePixelShader(bytes, None, Some(&mut shader))?;
                        device.CreateRenderTargetView(
                            &output,
                            Some(&D3D11_RENDER_TARGET_VIEW_DESC {
                                Format: plane,
                                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                                },
                            }),
                            Some(&mut target),
                        )?;
                        targets.push((
                            shader.context("创建像素转换shader")?,
                            target.context("创建像素转换目标平面")?,
                        ));
                    }
                    let mut vertex = None;
                    device.CreateVertexShader(
                        include_bytes!("encode_vs.cso"),
                        None,
                        Some(&mut vertex),
                    )?;
                    (
                        output,
                        Engine::Pixel {
                            vertex: vertex.context("创建像素转换顶点shader")?,
                            planes: targets,
                        },
                    )
                }
            };
            Ok(Self {
                inputs: Vec::with_capacity(4),
                timing: super::gpu_diagnostics::Timing::new(device, "encode conversion"),
                device: device.clone(),
                context: device.GetImmediateContext()?,
                engine,
                output,
                color,
                size,
            })
        }
    }
    /// Three full-resolution high-bit-aligned planes for NVENC's CUDA input.
    pub fn planar_44410(device: &ID3D11Device, size: (u32, u32), color: Color) -> Result<Self> {
        ensure!(
            size.0 > 0 && size.1 > 0 && size.0 <= 16384 && size.1 <= 16384 / 3,
            "4:4:4 10位输入尺寸超出GPU纹理限制"
        );
        unsafe {
            let mut output = None;
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: size.0,
                    Height: size.1 * 3,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_R16_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut output),
            )?;
            let output = output.context("创建44410平面纹理")?;
            let mut view = None;
            device.CreateUnorderedAccessView(&output, None, Some(&mut view))?;
            let bytes: &[u8] = if color.is_hdr() {
                include_bytes!("encode_44410_hdr.cso")
            } else {
                include_bytes!("encode_44410_sdr.cso")
            };
            let mut shader = None;
            device.CreateComputeShader(bytes, None, Some(&mut shader))?;
            Ok(Self {
                inputs: Vec::with_capacity(4),
                timing: super::gpu_diagnostics::Timing::new(device, "encode conversion"),
                device: device.clone(),
                context: device.GetImmediateContext()?,
                engine: Engine::Compute {
                    shader: shader.context("创建44410转换器")?,
                    views: vec![view],
                },
                output,
                size,
                color,
            })
        }
    }
    pub fn convert(&mut self, source: &ID3D11Texture2D) -> Result<()> {
        let _span = self.timing.as_mut().and_then(|timer| timer.begin());
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            source.GetDesc(&mut desc);
            ensure!(
                (desc.Width, desc.Height) == self.size,
                "编码输入尺寸与协商不一致"
            );
            let view = if let Some((_, view)) =
                self.inputs.iter().find(|(texture, _)| texture == source)
            {
                Some(view.clone())
            } else {
                let mut view = None;
                self.device
                    .CreateShaderResourceView(source, None, Some(&mut view))?;
                let view = view.context("编码输入视图")?;
                // Holding the resource prevents pointer reuse from aliasing an
                // old cache entry. Four entries cover both transfer rings.
                if self.inputs.len() == 4 {
                    self.inputs.remove(0);
                }
                self.inputs.push((source.clone(), view.clone()));
                Some(view)
            };
            match &self.engine {
                Engine::Compute { shader, views } => {
                    self.context.CSSetShader(shader, None);
                    self.context.CSSetShaderResources(0, Some(&[view]));
                    self.context.CSSetUnorderedAccessViews(
                        0,
                        views.len() as u32,
                        Some(views.as_ptr()),
                        None,
                    );
                    self.context
                        .Dispatch(self.size.0.div_ceil(8), self.size.1.div_ceil(8), 1);
                    self.context.CSSetShaderResources(0, Some(&[None]));
                    let empty = [None, None];
                    self.context.CSSetUnorderedAccessViews(
                        0,
                        views.len() as u32,
                        Some(empty.as_ptr()),
                        None,
                    );
                    self.context.CSSetShader(None, None);
                }
                Engine::Pixel { vertex, planes } => {
                    self.context.IASetInputLayout(None);
                    self.context
                        .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                    self.context.VSSetShader(vertex, None);
                    self.context.PSSetShaderResources(0, Some(&[view]));
                    for (index, (shader, target)) in planes.iter().enumerate() {
                        let divisor = if index == 1 { 2 } else { 1 };
                        self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                            Width: (self.size.0 / divisor) as f32,
                            Height: (self.size.1 / divisor) as f32,
                            MaxDepth: 1.,
                            ..Default::default()
                        }]));
                        self.context.PSSetShader(shader, None);
                        self.context
                            .OMSetRenderTargets(Some(&[Some(target.clone())]), None);
                        self.context.Draw(3, 0);
                    }
                    self.context.PSSetShaderResources(0, Some(&[None]));
                    self.context.OMSetRenderTargets(None, None);
                }
            }
        }
        Ok(())
    }
}
