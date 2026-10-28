//! D3D11 video texture shaders and geometry.
use super::swapchain::fit_rect;
use crate::media::video_color::RenderColor;
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, D3D_SRV_DIMENSION_TEXTURE2D,
    D3D_SRV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;
use winit::dpi::PhysicalSize;

pub(crate) struct VideoShaderRenderer {
    display_aspect: Option<(u32, u32)>,
    input_layout: ID3D11InputLayout,
    vertex_shader: ID3D11VertexShader,
    yuv_pixel_shader: ID3D11PixelShader,
    yuv_array_pixel_shader: ID3D11PixelShader,
    ayuv_pixel_shader: ID3D11PixelShader,
    ayuv_array_pixel_shader: ID3D11PixelShader,
    y410_pixel_shader: ID3D11PixelShader,
    y410_array_pixel_shader: ID3D11PixelShader,
    rgba_pixel_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    point_sampler: ID3D11SamplerState,
    rasterizer: ID3D11RasterizerState,
    color_buffer: Option<(RenderColor, u8, bool, ID3D11Buffer)>,
    geometry: Option<VideoShaderGeometry>,
    views: VecDeque<CachedVideoShaderViews>,
}

struct VideoShaderGeometry {
    key: VideoGeometryKey,
    vertex_buffer: ID3D11Buffer,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct VideoGeometryKey {
    display_aspect: Option<(u32, u32)>,
    visible_x: u32,
    visible_y: u32,
    coded_width: u32,
    coded_height: u32,
    width: u32,
    height: u32,
    output_width: u32,
    output_height: u32,
    content_top: u32,
}

struct CachedVideoShaderViews {
    texture_key: usize,
    array_slice: u32,
    format: DXGI_FORMAT,
    plane_0: ID3D11ShaderResourceView,
    plane_1: Option<ID3D11ShaderResourceView>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VideoVertex {
    position: [f32; 2],
    texcoord: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VideoColorTransform {
    rows: [[f32; 4]; 3],
    hdr: [f32; 4],
}

pub(crate) const SHARED_TEXTURE_CACHE_SIZE: usize = 32;

#[derive(Clone, Copy)]
pub(crate) struct VideoTextureView {
    pub(crate) color: RenderColor,
    pub(crate) array_slice: u32,
    pub(crate) input_format: DXGI_FORMAT,
    pub(crate) visible_x: u32,
    pub(crate) visible_y: u32,
    pub(crate) coded_width: u32,
    pub(crate) coded_height: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl VideoShaderRenderer {
    pub(crate) fn set_display_aspect(&mut self, aspect: Option<(u32, u32)>) {
        self.display_aspect = aspect;
    }
    pub(crate) fn set_rgba_shader(&mut self, shader: ID3D11PixelShader) {
        self.rgba_pixel_shader = shader;
    }
    const VERTEX_SHADER: &'static [u8] = include_bytes!("shaders/video_vs.cso");
    const YUV_PIXEL_SHADER: &'static [u8] = include_bytes!("shaders/video_yuv_ps.cso");
    const YUV_ARRAY_PIXEL_SHADER: &'static [u8] = include_bytes!("shaders/video_yuv_array_ps.cso");
    const RGBA_PIXEL_SHADER: &'static [u8] = include_bytes!("shaders/video_rgba_ps.cso");

    pub(crate) fn new(device: &ID3D11Device) -> Result<Self> {
        let input_elements = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: windows::core::s!("POSITION"),
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                ..Default::default()
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: windows::core::s!("TEXCOORD"),
                Format: DXGI_FORMAT_R32G32_FLOAT,
                AlignedByteOffset: 8,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                ..Default::default()
            },
        ];
        let mut input_layout = None;
        let mut vertex_shader = None;
        let mut yuv_pixel_shader = None;
        let mut yuv_array_pixel_shader = None;
        let mut ayuv_pixel_shader = None;
        let mut ayuv_array_pixel_shader = None;
        let mut y410_pixel_shader = None;
        let mut y410_array_pixel_shader = None;
        let mut rgba_pixel_shader = None;
        let mut sampler = None;
        let mut point_sampler = None;
        let mut rasterizer = None;
        unsafe {
            device.CreatePixelShader(
                include_bytes!("shaders/video_ayuv_ps.cso"),
                None,
                Some(&mut ayuv_pixel_shader),
            )?;
            device.CreatePixelShader(
                include_bytes!("shaders/video_ayuv_array_ps.cso"),
                None,
                Some(&mut ayuv_array_pixel_shader),
            )?;
            device.CreatePixelShader(
                include_bytes!("shaders/video_y410_ps.cso"),
                None,
                Some(&mut y410_pixel_shader),
            )?;
            device.CreatePixelShader(
                include_bytes!("shaders/video_y410_array_ps.cso"),
                None,
                Some(&mut y410_array_pixel_shader),
            )?;
            device.CreateInputLayout(
                &input_elements,
                Self::VERTEX_SHADER,
                Some(&mut input_layout),
            )?;
            device.CreateVertexShader(Self::VERTEX_SHADER, None, Some(&mut vertex_shader))?;
            device.CreatePixelShader(Self::YUV_PIXEL_SHADER, None, Some(&mut yuv_pixel_shader))?;
            device.CreatePixelShader(
                Self::YUV_ARRAY_PIXEL_SHADER,
                None,
                Some(&mut yuv_array_pixel_shader),
            )?;
            device.CreatePixelShader(
                Self::RGBA_PIXEL_SHADER,
                None,
                Some(&mut rgba_pixel_shader),
            )?;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut point_sampler),
            )?;
            device.CreateRasterizerState(
                &D3D11_RASTERIZER_DESC {
                    FillMode: D3D11_FILL_SOLID,
                    CullMode: D3D11_CULL_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                Some(&mut rasterizer),
            )?;
        }
        Ok(Self {
            input_layout: input_layout.context("D3D11 did not return the video input layout")?,
            vertex_shader: vertex_shader.context("D3D11 did not return the video vertex shader")?,
            yuv_pixel_shader: yuv_pixel_shader
                .context("D3D11 did not return the YUV pixel shader")?,
            yuv_array_pixel_shader: yuv_array_pixel_shader
                .context("D3D11 did not return the YUV array pixel shader")?,
            ayuv_pixel_shader: ayuv_pixel_shader.context("missing AYUV shader")?,
            ayuv_array_pixel_shader: ayuv_array_pixel_shader
                .context("missing AYUV array shader")?,
            y410_pixel_shader: y410_pixel_shader.context("missing Y410 shader")?,
            y410_array_pixel_shader: y410_array_pixel_shader
                .context("missing Y410 array shader")?,
            rgba_pixel_shader: rgba_pixel_shader
                .context("D3D11 did not return the RGBA pixel shader")?,
            sampler: sampler.context("D3D11 did not return the video sampler")?,
            point_sampler: point_sampler.context("D3D11 did not return the point sampler")?,
            rasterizer: rasterizer.context("D3D11 did not return the video rasterizer")?,
            color_buffer: None,
            display_aspect: None,
            geometry: None,
            views: VecDeque::with_capacity(SHARED_TEXTURE_CACHE_SIZE),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        render_target: &ID3D11RenderTargetView,
        texture: &ID3D11Texture2D,
        view: VideoTextureView,
        output_size: PhysicalSize<u32>,
        content_top: u32,
    ) -> Result<()> {
        // A former render target must be unbound before it becomes an input.
        unsafe {
            context.OMSetRenderTargets(None, None);
        }
        let (plane_0, plane_1) = self.shader_views(device, texture, &view)?;
        let geometry_key = VideoGeometryKey {
            display_aspect: self.display_aspect,
            visible_x: view.visible_x,
            visible_y: view.visible_y,
            coded_width: view.coded_width,
            coded_height: view.coded_height,
            width: view.width,
            height: view.height,
            output_width: output_size.width,
            output_height: output_size.height,
            content_top,
        };
        if self
            .geometry
            .as_ref()
            .is_none_or(|geometry| geometry.key != geometry_key)
        {
            let vertices = video_vertices(geometry_key);
            self.geometry = Some(VideoShaderGeometry {
                key: geometry_key,
                vertex_buffer: create_video_vertex_buffer(device, &vertices)?,
            });
        }
        let geometry = self
            .geometry
            .as_ref()
            .context("video shader geometry was not created")?;
        let yuv =
            plane_1.is_some() || matches!(view.input_format, DXGI_FORMAT_AYUV | DXGI_FORMAT_Y410);
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&raw mut desc) };
        let pixel_shader = if view.input_format == DXGI_FORMAT_AYUV {
            if desc.ArraySize > 1 {
                &self.ayuv_array_pixel_shader
            } else {
                &self.ayuv_pixel_shader
            }
        } else if view.input_format == DXGI_FORMAT_Y410 {
            if desc.ArraySize > 1 {
                &self.y410_array_pixel_shader
            } else {
                &self.y410_pixel_shader
            }
        } else if yuv && desc.ArraySize > 1 {
            &self.yuv_array_pixel_shader
        } else if yuv {
            &self.yuv_pixel_shader
        } else {
            &self.rgba_pixel_shader
        };
        let color_buffer = if yuv || view.color.hdr_peak_nits.is_some() {
            let bit_depth = if matches!(view.input_format, DXGI_FORMAT_P010 | DXGI_FORMAT_Y410) {
                10
            } else {
                8
            };
            let target_texture: ID3D11Texture2D = unsafe { render_target.GetResource() }?.cast()?;
            let mut target_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe {
                target_texture.GetDesc(&mut target_desc);
            }
            let hdr_output = target_desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
            if self
                .color_buffer
                .as_ref()
                .is_none_or(|(color, depth, output, _)| {
                    *color != view.color || *depth != bit_depth || *output != hdr_output
                })
            {
                let buffer = create_video_constant_buffer(
                    device,
                    VideoColorTransform {
                        rows: view.color.transform(bit_depth),
                        hdr: [
                            view.color
                                .hdr_peak_nits
                                .map_or(0.0, |_| if hdr_output { 1.0 } else { 2.0 }),
                            view.color.hdr_peak_nits.unwrap_or(1000) as f32,
                            0.0,
                            0.0,
                        ],
                    },
                )?;
                tracing::info!(color = ?view.color, bit_depth, "updated video YUV color transform");
                self.color_buffer = Some((view.color, bit_depth, hdr_output, buffer));
            }
            self.color_buffer
                .as_ref()
                .map(|(_, _, _, buffer)| buffer.clone())
        } else {
            None
        };
        let stride = std::mem::size_of::<VideoVertex>() as u32;
        let offset = 0_u32;
        let resources = [Some(plane_0), plane_1];
        // Select point sampling when the visible dimensions match the
        // video HWND within one pixel.
        let (display_width, display_height) = (view.width, view.height);
        let sampler = if display_width.abs_diff(output_size.width) <= 1
            && display_height.abs_diff(output_size.height.saturating_sub(content_top)) <= 1
        {
            &self.point_sampler
        } else {
            &self.sampler
        };
        unsafe {
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.IASetInputLayout(&self.input_layout);
            context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(geometry.vertex_buffer.clone())),
                Some(&stride),
                Some(&offset),
            );
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(pixel_shader, None);
            context.PSSetShaderResources(0, Some(&resources));
            context.PSSetSamplers(0, Some(&[Some(sampler.clone())]));
            context.PSSetConstantBuffers(0, Some(&[color_buffer]));
            context.RSSetState(&self.rasterizer);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: output_size.width as f32,
                Height: output_size.height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            context.OMSetRenderTargets(Some(&[Some(render_target.clone())]), None);
            context.OMSetBlendState(None, Some(&[0.0; 4]), u32::MAX);
            context.Draw(4, 0);
            context.PSSetShaderResources(0, Some(&[None, None]));
        }
        Ok(())
    }

    fn shader_views(
        &mut self,
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
        view: &VideoTextureView,
    ) -> Result<(ID3D11ShaderResourceView, Option<ID3D11ShaderResourceView>)> {
        let texture_key = texture.as_raw() as usize;
        if let Some(index) = self.views.iter().position(|cached| {
            cached.texture_key == texture_key
                && cached.array_slice == view.array_slice
                && cached.format == view.input_format
        }) {
            let cached = self.views.remove(index).expect("checked SRV cache index");
            let result = (cached.plane_0.clone(), cached.plane_1.clone());
            self.views.push_back(cached);
            return Ok(result);
        }
        let (y_format, uv_format) = match view.input_format {
            DXGI_FORMAT_NV12 => (DXGI_FORMAT_R8_UNORM, Some(DXGI_FORMAT_R8G8_UNORM)),
            DXGI_FORMAT_P010 => (DXGI_FORMAT_R16_UNORM, Some(DXGI_FORMAT_R16G16_UNORM)),
            DXGI_FORMAT_AYUV => (DXGI_FORMAT_R8G8B8A8_UNORM, None),
            DXGI_FORMAT_Y410 => (DXGI_FORMAT_R10G10B10A2_UNORM, None),
            DXGI_FORMAT_R8G8B8A8_UNORM => (DXGI_FORMAT_R8G8B8A8_UNORM, None),
            DXGI_FORMAT_R16G16B16A16_FLOAT => (DXGI_FORMAT_R16G16B16A16_FLOAT, None),
            format => bail!("unsupported D3D11 shader input format {format:?}"),
        };
        let plane_0 =
            create_video_shader_resource_view(device, texture, y_format, view.array_slice)?;
        let plane_1 = uv_format
            .map(|format| {
                create_video_shader_resource_view(device, texture, format, view.array_slice)
            })
            .transpose()?;
        self.views.push_back(CachedVideoShaderViews {
            texture_key,
            array_slice: view.array_slice,
            format: view.input_format,
            plane_0: plane_0.clone(),
            plane_1: plane_1.clone(),
        });
        while self.views.len() > SHARED_TEXTURE_CACHE_SIZE {
            self.views.pop_front();
        }
        Ok((plane_0, plane_1))
    }

    pub(crate) fn reset_geometry(&mut self) {
        self.geometry = None;
    }

    pub(crate) fn reset_input_cache(&mut self) {
        self.geometry = None;
        self.views.clear();
    }
}

fn create_video_vertex_buffer(
    device: &ID3D11Device,
    vertices: &[VideoVertex; 4],
) -> Result<ID3D11Buffer> {
    let mut buffer = None;
    unsafe {
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of_val(vertices) as u32,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
                ..Default::default()
            },
            Some(&D3D11_SUBRESOURCE_DATA {
                pSysMem: vertices.as_ptr().cast(),
                ..Default::default()
            }),
            Some(&mut buffer),
        )
    }
    .context("create video quad vertex buffer")?;
    buffer.context("D3D11 did not return the video quad vertex buffer")
}

fn create_video_constant_buffer(
    device: &ID3D11Device,
    transform: VideoColorTransform,
) -> Result<ID3D11Buffer> {
    let mut buffer = None;
    unsafe {
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<VideoColorTransform>() as u32,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            },
            Some(&D3D11_SUBRESOURCE_DATA {
                pSysMem: (&raw const transform).cast(),
                ..Default::default()
            }),
            Some(&mut buffer),
        )
    }
    .context("create video color-transform constant buffer")?;
    buffer.context("D3D11 did not return the video color-transform buffer")
}

fn create_video_shader_resource_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    array_slice: u32,
) -> Result<ID3D11ShaderResourceView> {
    let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&raw mut texture_desc) };
    let (dimension, anonymous) = if texture_desc.ArraySize > 1 {
        (
            D3D_SRV_DIMENSION_TEXTURE2DARRAY,
            D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: array_slice,
                    ArraySize: 1,
                },
            },
        )
    } else {
        (
            D3D_SRV_DIMENSION_TEXTURE2D,
            D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        )
    };
    let mut resource_view = None;
    unsafe {
        device.CreateShaderResourceView(
            texture,
            Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: dimension,
                Anonymous: anonymous,
            }),
            Some(&mut resource_view),
        )
    }
    .with_context(|| format!("create D3D11 video plane SRV {format:?}"))?;
    resource_view.context("D3D11 did not return the video plane SRV")
}

fn video_vertices(key: VideoGeometryKey) -> [VideoVertex; 4] {
    let (display_width, display_height) = (key.width, key.height);
    let (display_width, display_height) = key
        .display_aspect
        .unwrap_or((display_width, display_height));
    let mut destination = fit_rect(
        display_width,
        display_height,
        key.output_width,
        key.output_height.saturating_sub(key.content_top).max(1),
    );
    destination.top += key.content_top as i32;
    destination.bottom += key.content_top as i32;
    let left = destination.left as f32 / key.output_width.max(1) as f32 * 2.0 - 1.0;
    let right = destination.right as f32 / key.output_width.max(1) as f32 * 2.0 - 1.0;
    let top = 1.0 - destination.top as f32 / key.output_height.max(1) as f32 * 2.0;
    let bottom = 1.0 - destination.bottom as f32 / key.output_height.max(1) as f32 * 2.0;
    let u0 = key.visible_x as f32 / key.coded_width.max(1) as f32;
    let v0 = key.visible_y as f32 / key.coded_height.max(1) as f32;
    let u1 = key.visible_x.saturating_add(key.width) as f32 / key.coded_width.max(1) as f32;
    let v1 = key.visible_y.saturating_add(key.height) as f32 / key.coded_height.max(1) as f32;
    let texcoords = [[u0, v0], [u1, v0], [u0, v1], [u1, v1]];
    [
        VideoVertex {
            position: [left, top],
            texcoord: texcoords[0],
        },
        VideoVertex {
            position: [right, top],
            texcoord: texcoords[1],
        },
        VideoVertex {
            position: [left, bottom],
            texcoord: texcoords[2],
        },
        VideoVertex {
            position: [right, bottom],
            texcoord: texcoords[3],
        },
    ]
}
