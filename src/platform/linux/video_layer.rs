//! The video layer drawn underneath the egui chrome on Linux.
//!
//! Decoded frames arrive as NV12, converted to RGB by the fragment shader, or
//! as RGBA when the decoder produced 4:4:4 and the CPU already converted it.
//! NV12 normally arrives as a dmabuf the layer imports as two single-plane
//! textures, so no frame data crosses the CPU; drivers that cannot export one
//! are uploaded instead. Keeping NV12 on the GPU saves both the conversion and
//! two thirds of the upload: a 2560x1600 frame is 6 MiB instead of 16 MiB.
//!
//! The quad is letterboxed into the content area and can be rotated by the
//! multiples of 90 degrees the remote display reports.
use anyhow::Result;
use wgpu::util::DeviceExt;

/// Where the video sits inside the window, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VideoPlacement {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    /// Clockwise rotation in degrees: 0, 90, 180 or 270.
    pub(crate) rotation: u16,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniform {
    /// Quad rect in clip space: offset then half-extent.
    offset: [f32; 2],
    extent: [f32; 2],
    /// Texture-space rotation matrix, column major.
    rotation: [f32; 4],
    /// Affine YUV to RGB rows, one per colour component.
    transform: [[f32; 4]; 3],
}

/// A pipeline and the bind group layout its frames are bound with.
struct Program {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

enum Planes {
    Rgba(wgpu::Texture),
    Nv12 {
        luma: wgpu::Texture,
        chroma: wgpu::Texture,
    },
}

/// An NV12 picture whose planes are dmabuf-backed textures.
///
/// The textures alias the decoder's surface, so the exported frame must stay
/// alive for as long as they are drawn. `lease` carries that reservation.
pub(crate) struct DmabufPlanes {
    pub(crate) luma: wgpu::Texture,
    pub(crate) chroma: wgpu::Texture,
    pub(crate) lease: crate::platform::linux::dmabuf::SurfaceLease,
}

impl DmabufPlanes {
    /// Wraps the decoder's exported dmabuf as two sampled textures.
    ///
    /// Returns `None` when the backend is not Vulkan or the driver cannot
    /// import the buffer, so the caller can fall back to the CPU upload.
    pub(crate) fn new(
        device: &wgpu::Device,
        frame: &crate::platform::linux::dmabuf::DmabufFrame,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        use wgpu::hal::api::Vulkan as Vk;

        // The dmabuf import path is Vulkan-only; the GL backend keeps the
        // CPU upload even when the decoder exported a buffer.
        let hal_device = unsafe { device.as_hal::<Vk>() }?;
        let luma = import_plane(
            device,
            &hal_device,
            &frame.planes[0],
            frame.modifier,
            wgpu::TextureFormat::R8Unorm,
            width,
            height,
        )?;
        let chroma = import_plane(
            device,
            &hal_device,
            &frame.planes[1],
            frame.modifier,
            wgpu::TextureFormat::Rg8Unorm,
            width.div_ceil(2),
            height.div_ceil(2),
        )?;
        Some(Self {
            luma,
            chroma,
            lease: frame.lease(),
        })
    }
}

/// Imports one dmabuf plane as a single-plane sampled texture.
fn import_plane(
    device: &wgpu::Device,
    hal_device: &wgpu::hal::vulkan::Device,
    plane: &crate::platform::linux::dmabuf::Plane,
    modifier: u64,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> Option<wgpu::Texture> {
    use wgpu::hal::api::Vulkan as Vk;
    let fd = plane.dup().ok()?;
    // wgpu-hal wants its own descriptor type, with the usage already reduced to
    // the `TextureUses` bits the resource tracker understands.
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some("openuuyc-video-dmabuf"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUses::RESOURCE,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    let desc = wgpu::TextureDescriptor {
        label: Some("openuuyc-video-dmabuf"),
        size: hal_desc.size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    let hal_texture = unsafe {
        hal_device
            .texture_from_dmabuf_fd(fd, &hal_desc, modifier, plane.stride, plane.offset)
            .ok()?
    };
    // Safe: the hal texture was created from this device and matches `desc`.
    Some(unsafe {
        device.create_texture_from_hal::<Vk>(hal_texture, &desc, wgpu::TextureUses::RESOURCE)
    })
}

struct Frame {
    planes: Planes,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    /// Held so the decoder's surface stays reserved while the texture is live.
    lease: Option<crate::platform::linux::dmabuf::SurfaceLease>,
}

impl Frame {
    const fn is_nv12(&self) -> bool {
        matches!(self.planes, Planes::Nv12 { .. })
    }
}

pub(crate) struct VideoLayer {
    rgba: Program,
    nv12: Program,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    frame: Option<Frame>,
}

impl VideoLayer {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("openuuyc-video"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
        });
        let rgba = Program::new(device, &module, format, "fs_rgba", 1);
        let nv12 = Program::new(device, &module, format, "fs_nv12", 2);
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("openuuyc-video"),
            contents: bytemuck::bytes_of(&Uniform {
                offset: [0.0, 0.0],
                extent: [1.0, 1.0],
                rotation: [1.0, 0.0, 0.0, 1.0],
                transform: [[0.0; 4]; 3],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("openuuyc-video"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            rgba,
            nv12,
            sampler,
            uniform,
            frame: None,
        }
    }

    pub(crate) fn has_frame(&self) -> bool {
        self.frame.is_some()
    }

    /// Whether the current picture aliases a decoder surface.
    ///
    /// The caller must not let that surface return to the decoder pool until
    /// the GPU is done sampling it, so it waits for the submission to complete.
    pub(crate) fn holds_dmabuf(&self) -> bool {
        self.frame
            .as_ref()
            .is_some_and(|frame| frame.lease.is_some())
    }

    pub(crate) fn clear(&mut self) {
        self.frame = None;
    }

    /// Binds an exported dmabuf as the current picture.
    ///
    /// The luma and chroma textures alias the decoder's surface, so their
    /// lease rides along in the frame and is only released when the picture is
    /// replaced or cleared. Returns `false` when the buffer could not be
    /// imported, letting the caller fall back to the CPU upload.
    pub(crate) fn upload_dmabuf(
        &mut self,
        device: &wgpu::Device,
        frame: &crate::platform::linux::dmabuf::DmabufFrame,
        width: u32,
        height: u32,
    ) -> bool {
        let Some(planes) = DmabufPlanes::new(device, frame, width, height) else {
            return false;
        };
        let luma_view = planes
            .luma
            .create_view(&wgpu::TextureViewDescriptor::default());
        let chroma_view = planes
            .chroma
            .create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("openuuyc-video-nv12"),
            layout: &self.nv12.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&luma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&chroma_view),
                },
            ],
        });
        self.frame = Some(Frame {
            planes: Planes::Nv12 {
                luma: planes.luma,
                chroma: planes.chroma,
            },
            bind_group,
            width,
            height,
            lease: Some(planes.lease),
        });
        true
    }

    /// `pixels` is tightly packed RGBA8, `width * height` entries.
    pub(crate) fn upload_rgba(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<()> {
        anyhow::ensure!(
            width > 0 && height > 0 && pixels.len() >= (width as usize * height as usize * 4),
            "视频帧尺寸与数据不匹配"
        );
        let reuse = matches!(
            &self.frame,
            Some(frame) if !frame.is_nv12() && frame.width == width && frame.height == height
        );
        if !reuse {
            let texture = plane(device, width, height, wgpu::TextureFormat::Rgba8Unorm);
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("openuuyc-video-rgba"),
                layout: &self.rgba.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.frame = Some(Frame {
                planes: Planes::Rgba(texture),
                bind_group,
                width,
                height,
                lease: None,
            });
        }
        let Some(Frame {
            planes: Planes::Rgba(texture),
            ..
        }) = self.frame.as_ref()
        else {
            anyhow::bail!("视频纹理不可用");
        };
        write_plane(queue, texture, width, height, 4, pixels);
        Ok(())
    }

    /// `data` is packed NV12: a `width * height` luma plane followed by an
    /// interleaved CbCr plane at half resolution.
    pub(crate) fn upload_nv12(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<()> {
        let chroma_height = height.div_ceil(2);
        let chroma_width = width.div_ceil(2);
        let luma_bytes = width as usize * height as usize;
        anyhow::ensure!(
            width > 0
                && height > 0
                && data.len() >= luma_bytes + chroma_width as usize * chroma_height as usize * 2,
            "视频帧尺寸与数据不匹配"
        );
        let reuse = matches!(
            &self.frame,
            Some(frame) if frame.is_nv12() && frame.width == width && frame.height == height
        );
        if !reuse {
            let luma = plane(device, width, height, wgpu::TextureFormat::R8Unorm);
            let chroma = plane(
                device,
                chroma_width,
                chroma_height,
                wgpu::TextureFormat::Rg8Unorm,
            );
            let luma_view = luma.create_view(&wgpu::TextureViewDescriptor::default());
            let chroma_view = chroma.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("openuuyc-video-nv12"),
                layout: &self.nv12.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&luma_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&chroma_view),
                    },
                ],
            });
            self.frame = Some(Frame {
                planes: Planes::Nv12 { luma, chroma },
                bind_group,
                width,
                height,
                lease: None,
            });
        }
        let Some(Frame {
            planes: Planes::Nv12 { luma, chroma },
            ..
        }) = self.frame.as_ref()
        else {
            anyhow::bail!("视频纹理不可用");
        };
        write_plane(queue, luma, width, height, 1, &data[..luma_bytes]);
        write_plane(
            queue,
            chroma,
            chroma_width,
            chroma_height,
            2,
            &data[luma_bytes..],
        );
        Ok(())
    }

    pub(crate) fn draw(
        &self,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'static>,
        target: (u32, u32),
        placement: VideoPlacement,
        transform: [[f32; 4]; 3],
    ) {
        let (Some(frame), true) = (self.frame.as_ref(), target.0 > 0 && target.1 > 0) else {
            return;
        };
        let width = target.0 as f32;
        let height = target.1 as f32;
        // Clip space is [-1, 1] with y up; the placement is in pixels with y down.
        let center_x = (placement.x + placement.width / 2.0) / width * 2.0 - 1.0;
        let center_y = 1.0 - (placement.y + placement.height / 2.0) / height * 2.0;
        let rotation = uv_rotation(placement.rotation);
        queue.write_buffer(
            &self.uniform,
            0,
            bytemuck::bytes_of(&Uniform {
                offset: [center_x, center_y],
                extent: [placement.width / width, placement.height / height],
                rotation,
                transform,
            }),
        );
        let program = if frame.is_nv12() {
            &self.nv12
        } else {
            &self.rgba
        };
        pass.set_pipeline(&program.pipeline);
        pass.set_bind_group(0, &frame.bind_group, &[]);
        pass.draw(0..4, 0..1);
    }
}

impl Program {
    fn new(
        device: &wgpu::Device,
        module: &wgpu::ShaderModule,
        format: wgpu::TextureFormat,
        entry_point: &str,
        planes: u32,
    ) -> Self {
        let mut entries = vec![
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ];
        if planes > 1 {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            });
        }
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("openuuyc-video"),
            entries: &entries,
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("openuuyc-video"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("openuuyc-video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some(entry_point),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline, layout }
    }
}

fn plane(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("openuuyc-video"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    bytes_per_texel: u32,
    data: &[u8],
) {
    let needed = (width * bytes_per_texel) as usize * height as usize;
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &data[..needed],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * bytes_per_texel),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

/// 屏幕方向 → 纹理坐标的旋转矩阵（列主序，和着色器里 `mat2x2` 的写法一致）。
///
/// 这里的角度是「把画面顺时针转这么多」（CVO/WebRTC 的约定，Windows 那条路就是这么
/// 用的）；而着色器算的是**采样**用的 uv，也就是画面变换的逆，所以 90 度对应的是逆时针
/// 那个矩阵。写成顺时针会让转过 90°/270° 的被控屏在 Linux 主控上整体差 180°——
/// 0°/180° 两个方向一样，所以只有那两档会看出来。
fn uv_rotation(degrees: u16) -> [f32; 4] {
    match degrees % 360 {
        90 => [0.0, -1.0, 1.0, 0.0],
        180 => [-1.0, 0.0, 0.0, -1.0],
        270 => [0.0, 1.0, -1.0, 0.0],
        _ => [1.0, 0.0, 0.0, 1.0],
    }
}

const SHADER: &str = r#"
struct Uniform {
    offset: vec2<f32>,
    extent: vec2<f32>,
    rotation: vec4<f32>,
    transform0: vec4<f32>,
    transform1: vec4<f32>,
    transform2: vec4<f32>,
};

@group(0) @binding(0) var<uniform> settings: Uniform;
@group(0) @binding(1) var plane0: texture_2d<f32>;
@group(0) @binding(2) var plane_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // Triangle strip over the unit quad: (0,0) (1,0) (0,1) (1,1).
    let corner = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    // Clip space has y up while the placement and the texture have y down.
    let centered = vec2<f32>(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0);
    var out: VertexOutput;
    out.position = vec4<f32>(settings.offset + centered * settings.extent, 0.0, 1.0);
    // Rotate around the texture centre, then move back into [0, 1].
    let matrix = mat2x2<f32>(
        settings.rotation.x, settings.rotation.y,
        settings.rotation.z, settings.rotation.w,
    );
    let rotated = matrix * (corner - vec2<f32>(0.5, 0.5));
    out.uv = rotated + vec2<f32>(0.5, 0.5);
    return out;
}

@fragment
fn fs_rgba(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(plane0, plane_sampler, in.uv);
}

@group(0) @binding(3) var plane1: texture_2d<f32>;

@fragment
fn fs_nv12(in: VertexOutput) -> @location(0) vec4<f32> {
    let luma = textureSample(plane0, plane_sampler, in.uv).r;
    let chroma = textureSample(plane1, plane_sampler, in.uv).rg;
    let yuv = vec4<f32>(luma, chroma.r, chroma.g, 1.0);
    let rgb = vec3<f32>(
        dot(settings.transform0, yuv),
        dot(settings.transform1, yuv),
        dot(settings.transform2, yuv),
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::uv_rotation;

    /// 屏幕四角的 `corner`（和顶点着色器里一样：左上、右上、左下、右下）各采到纹理的哪一点。
    fn sampled_corners(degrees: u16) -> [(f32, f32); 4] {
        let matrix = uv_rotation(degrees);
        [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].map(|(x, y)| {
            let (dx, dy) = (x - 0.5, y - 0.5);
            (
                matrix[0] * dx + matrix[2] * dy + 0.5,
                matrix[1] * dx + matrix[3] * dy + 0.5,
            )
        })
    }

    fn same(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
    }

    /// 报的是「顺时针转多少」，画面就得顺时针转：90 度时屏幕左上角采到的是纹理左下角
    /// （顺时针转 90° 正是把左下角转到左上角），270 度反过来。写反了整幅画差 180°。
    #[test]
    fn a_clockwise_angle_turns_the_picture_clockwise() {
        let corners = sampled_corners(90);
        assert!(same(corners[0], (0.0, 1.0)), "{corners:?}");
        assert!(same(corners[1], (0.0, 0.0)), "{corners:?}");
        let corners = sampled_corners(270);
        assert!(same(corners[0], (1.0, 0.0)), "{corners:?}");
        assert!(same(corners[1], (1.0, 1.0)), "{corners:?}");
        // 0 和 180 两个方向一样，只钉住它们的落点。
        assert!(same(sampled_corners(0)[0], (0.0, 0.0)));
        assert!(same(sampled_corners(180)[0], (1.0, 1.0)));
    }
}
