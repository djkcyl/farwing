//! Independent EGL/GLES RGB -> BT.601 limited-range NV12 conversion.
//!
//! EGL objects stay on the PipeWire thread. Only application-owned, completed
//! NV12 allocations cross threads; compositor DMA-BUFs never leave a callback.
use anyhow::{Context, Result, bail, ensure};
use glow::HasContext;
use std::{
    ffi::{CString, c_void},
    os::fd::AsRawFd,
    rc::Rc,
    sync::Arc,
};

type Handle = *mut c_void;
type GetProc = unsafe extern "C" fn(*const std::ffi::c_char) -> Handle;
type CreateImage = unsafe extern "C" fn(Handle, Handle, u32, Handle, *const i32) -> Handle;
type DestroyImage = unsafe extern "C" fn(Handle, Handle) -> u32;
type BindImage = unsafe extern "C" fn(u32, Handle);
type MakeCurrent = unsafe extern "C" fn(Handle, Handle, Handle, Handle) -> u32;
type DestroyContext = unsafe extern "C" fn(Handle, Handle) -> u32;
type Terminate = unsafe extern "C" fn(Handle) -> u32;
type DestroyGbm = unsafe extern "C" fn(Handle);

// Native-size sampling stays an exact `texelFetch` (the original behaviour).
// When the target geometry differs, the shader scales during the conversion so
// the frame never has to be read back to the CPU. Chroma averages the same 2x2
// luma block the CPU path did, only at the scaled positions.
const Y_SHADER: &str = "#version 300 es\nprecision highp float;uniform sampler2D rgb;uniform ivec2 origin;uniform ivec2 src;uniform ivec2 dst;layout(location=0) out vec4 outColor;void main(){vec3 c;if(src==dst){c=texelFetch(rgb,origin+ivec2(gl_FragCoord.xy),0).rgb;}else{vec2 s=vec2(src)/vec2(dst);vec2 p=vec2(origin)+gl_FragCoord.xy*s;c=texture(rgb,p/vec2(textureSize(rgb,0))).rgb;}outColor=vec4((16.0+dot(c,vec3(65.481,128.553,24.966)))/255.0,0,0,1);}";
const UV_SHADER: &str = "#version 300 es\nprecision highp float;uniform sampler2D rgb;uniform ivec2 origin;uniform ivec2 src;uniform ivec2 dst;layout(location=0) out vec4 outColor;void main(){vec3 c;if(src==dst){ivec2 p=origin+ivec2(gl_FragCoord.xy)*2;c=(texelFetch(rgb,p,0).rgb+texelFetch(rgb,p+ivec2(1,0),0).rgb+texelFetch(rgb,p+ivec2(0,1),0).rgb+texelFetch(rgb,p+ivec2(1,1),0).rgb)*0.25;}else{vec2 s=vec2(src)/vec2(dst);vec2 sz=vec2(textureSize(rgb,0));vec2 o=vec2(origin);vec2 b=2.0*gl_FragCoord.xy-0.5;c=(texture(rgb,(o+b*s)/sz).rgb+texture(rgb,(o+(b+vec2(1.0,0.0))*s)/sz).rgb+texture(rgb,(o+(b+vec2(0.0,1.0))*s)/sz).rgb+texture(rgb,(o+(b+vec2(1.0,1.0))*s)/sz).rgb)*0.25;}outColor=vec4((128.0+dot(c,vec3(-37.797,-74.203,112.0)))/255.0,(128.0+dot(c,vec3(112.0,-93.786,-18.214)))/255.0,0,1);}";

/// Exported allocation is immutable after GPU completion. FDs own its memory
/// even after the allocating VA surface is destroyed; no pool reuses it.
pub(crate) struct OwnedNv12(pub cros_libva::DrmPrimeSurfaceDescriptor, bool);
impl OwnedNv12 {
    pub fn size(&self) -> (u32, u32) {
        (self.0.width, self.0.height)
    }

    pub fn import(
        self: &Arc<Self>,
        display: &Rc<cros_libva::Display>,
    ) -> Result<cros_libva::Surface<Nv12Import>> {
        let p = &self.0;
        ensure!(
            p.fourcc == cros_libva::VA_FOURCC_NV12
                && p.objects.len() == 1
                && p.layers.len() == 1
                && p.layers[0].num_planes == 2
                && p.layers[0].object_index[..2] == [0, 0],
            "非单对象 NV12 布局"
        );
        Ok(display
            .create_surfaces(
                cros_libva::VA_RT_FORMAT_YUV420,
                Some(cros_libva::VA_FOURCC_NV12),
                p.width,
                p.height,
                None,
                vec![Nv12Import {
                    frame: self.clone(),
                    prime2: self.1,
                }],
            )?
            .remove(0))
    }

    pub fn pixels(self: &Arc<Self>) -> Result<Vec<u8>> {
        // Fallback only; no VA/EGL context or Rc crosses a thread boundary.
        thread_local! { static DISPLAY: std::cell::OnceCell<Rc<cros_libva::Display>> = const { std::cell::OnceCell::new() }; }
        DISPLAY.with(|cell| {
            if cell.get().is_none() {
                super::cuda::initialize_before_va();
                let _ = cell.set(cros_libva::Display::open().context("无 VA 显示")?);
            }
            let surface = self.import(cell.get().unwrap())?;
            let image = cros_libva::Image::derive_from(&surface, self.size())?;
            let p = image.image();
            ensure!(
                p.format.fourcc == cros_libva::VA_FOURCC_NV12,
                "NV12 回读格式变化"
            );
            let data = image.as_ref();
            let (w, h) = self.size();
            let ylen = p.pitches[0] as usize * h as usize;
            let uvlen = p.pitches[1] as usize * (h as usize / 2);
            let y = data
                .get(p.offsets[0] as usize..p.offsets[0] as usize + ylen)
                .context("Y 越界")?;
            let uv = data
                .get(p.offsets[1] as usize..p.offsets[1] as usize + uvlen)
                .context("UV 越界")?;
            let mut pixels = vec![0; w as usize * h as usize * 4];
            yuv::yuv_nv12_to_bgra(
                &yuv::YuvBiPlanarImage {
                    y_plane: y,
                    y_stride: p.pitches[0],
                    uv_plane: uv,
                    uv_stride: p.pitches[1],
                    width: w,
                    height: h,
                },
                &mut pixels,
                w * 4,
                yuv::YuvRange::Limited,
                yuv::YuvStandardMatrix::Bt601,
                yuv::YuvConversionMode::Balanced,
            )?;
            Ok(pixels)
        })
    }
}

pub(crate) struct Nv12Import {
    frame: Arc<OwnedNv12>,
    prime2: bool,
}
impl cros_libva::SurfaceMemoryDescriptor for Nv12Import {
    fn add_attrs(
        &mut self,
        attrs: &mut Vec<cros_libva::VASurfaceAttrib>,
    ) -> Option<Box<dyn std::any::Any>> {
        let p = &self.frame.0;
        let mut memory =
            cros_libva::VASurfaceAttrib::new_memory_type(cros_libva::MemoryType::DrmPrime2);
        if self.prime2 {
            let mut d: Box<cros_libva::VADRMPRIMESurfaceDescriptor> = Box::default();
            d.fourcc = p.fourcc;
            d.width = p.width;
            d.height = p.height;
            d.num_objects = 1;
            d.num_layers = 1;
            d.objects[0].fd = p.objects[0].fd.as_raw_fd();
            d.objects[0].size = p.objects[0].size;
            d.objects[0].drm_format_modifier = p.objects[0].drm_format_modifier;
            d.layers[0].drm_format = p.layers[0].drm_format;
            d.layers[0].num_planes = 2;
            d.layers[0].offset = p.layers[0].offset;
            d.layers[0].pitch = p.layers[0].pitch;
            attrs.push(memory);
            attrs.push(cros_libva::VASurfaceAttrib::new_buffer_descriptor(&mut *d));
            Some(d)
        } else {
            let fds = Box::new([p.objects[0].fd.as_raw_fd() as usize]);
            let mut d: Box<cros_libva::VASurfaceAttribExternalBuffers> = Box::default();
            d.pixel_format = p.fourcc;
            d.width = p.width;
            d.height = p.height;
            d.data_size = p.objects[0].size;
            d.num_planes = 2;
            d.pitches = p.layers[0].pitch;
            d.offsets = p.layers[0].offset;
            d.buffers = fds.as_ptr() as *mut usize;
            d.num_buffers = 1;
            memory.value.value.i = cros_libva::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME as i32;
            attrs.push(memory);
            attrs.push(cros_libva::VASurfaceAttrib::new_buffer_descriptor(&mut *d));
            Some(Box::new((d, fds)))
        }
    }
}

pub(crate) struct RgbDma {
    pub fd: i32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub offset: u32,
    pub fourcc: u32,
    pub modifier: u64,
}

/// Thread-bound resources. Drop is always on the creating thread.
pub(crate) struct Converter {
    _egl: libloading::Library,
    _gbm: libloading::Library,
    display: Handle,
    context: Handle,
    gbm: Handle,
    make_current: MakeCurrent,
    destroy_context: DestroyContext,
    terminate: Terminate,
    destroy_gbm: DestroyGbm,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    bind_image: BindImage,
    gl: Option<glow::Context>,
    programs: Vec<glow::NativeProgram>,
    va: Rc<cros_libva::Display>,
    prime2: bool,
}

impl Converter {
    pub fn new(va: Rc<cros_libva::Display>) -> Result<Self> {
        let mut config = va.create_config(
            vec![],
            cros_libva::VAProfile::VAProfileNone,
            cros_libva::VAEntrypoint::VAEntrypointVideoProc,
        )?;
        let types = config
            .query_surface_attributes_by_type(
                cros_libva::VASurfaceAttribType::VASurfaceAttribMemoryType,
            )?
            .iter()
            .fold(0u32, |v, a| match a {
                cros_libva::GenericValue::Integer(i) => v | *i as u32,
                _ => v,
            });
        let prime2 = types & cros_libva::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 != 0;
        ensure!(
            prime2 || types & cros_libva::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME != 0,
            "无 NV12 PRIME 导入能力"
        );
        // All dynamically loaded functions have EGL/GBM's documented C ABI.
        // Libraries outlive their symbols, contexts, and exported image views.
        unsafe {
            let egl = libloading::Library::new("libEGL.so.1")?;
            let gbmlib = libloading::Library::new("libgbm.so.1")?;
            let get_proc: GetProc = *egl.get(b"eglGetProcAddress\0")?;
            macro_rules! ext {
                ($name:literal, $ty:ty) => {{
                    let p = get_proc(concat!($name, "\0").as_ptr().cast());
                    ensure!(!p.is_null(), concat!("缺少 ", $name));
                    std::mem::transmute::<Handle, $ty>(p)
                }};
            }
            let platform = ext!(
                "eglGetPlatformDisplayEXT",
                unsafe extern "C" fn(u32, Handle, *const i32) -> Handle
            );
            let create_gbm: unsafe extern "C" fn(i32) -> Handle =
                *gbmlib.get(b"gbm_create_device\0")?;
            // Resolve everything before allocating native resources.
            let mut out = Self {
                make_current: *egl.get(b"eglMakeCurrent\0")?,
                destroy_context: *egl.get(b"eglDestroyContext\0")?,
                terminate: *egl.get(b"eglTerminate\0")?,
                destroy_gbm: *gbmlib.get(b"gbm_device_destroy\0")?,
                create_image: ext!("eglCreateImageKHR", CreateImage),
                destroy_image: ext!("eglDestroyImageKHR", DestroyImage),
                bind_image: ext!("glEGLImageTargetTexture2DOES", BindImage),
                display: std::ptr::null_mut(),
                context: std::ptr::null_mut(),
                gbm: std::ptr::null_mut(),
                gl: None,
                programs: vec![],
                va,
                prime2,
                _egl: egl,
                _gbm: gbmlib,
            };
            out.gbm = create_gbm(out.va.drm_fd());
            ensure!(!out.gbm.is_null(), "创建 GBM 失败");
            out.display = platform(0x31d7, out.gbm, std::ptr::null());
            ensure!(!out.display.is_null(), "创建 EGL 显示失败");
            let initialize: unsafe extern "C" fn(Handle, *mut i32, *mut i32) -> u32 =
                *out._egl.get(b"eglInitialize\0")?;
            let bind_api: unsafe extern "C" fn(u32) -> u32 = *out._egl.get(b"eglBindAPI\0")?;
            let choose: unsafe extern "C" fn(
                Handle,
                *const i32,
                *mut Handle,
                i32,
                *mut i32,
            ) -> u32 = *out._egl.get(b"eglChooseConfig\0")?;
            let create: unsafe extern "C" fn(Handle, Handle, Handle, *const i32) -> Handle =
                *out._egl.get(b"eglCreateContext\0")?;
            ensure!(
                initialize(out.display, std::ptr::null_mut(), std::ptr::null_mut()) != 0
                    && bind_api(0x30a0) != 0,
                "初始化 EGL 失败"
            );
            let mut config = std::ptr::null_mut();
            let mut n = 0;
            ensure!(
                choose(
                    out.display,
                    [0x3040, 0x40, 0x3038].as_ptr(),
                    &mut config,
                    1,
                    &mut n
                ) != 0
                    && n > 0,
                "无 GLES3 config"
            );
            out.context = create(
                out.display,
                config,
                std::ptr::null_mut(),
                [0x3098, 3, 0x3038].as_ptr(),
            );
            ensure!(!out.context.is_null(), "创建 GLES3 失败");
            ensure!(
                (out.make_current)(
                    out.display,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    out.context
                ) != 0,
                "绑定 GLES3 失败"
            );
            out.gl = Some(glow::Context::from_loader_function(|s| {
                get_proc(CString::new(s).unwrap().as_ptr())
            }));
            out.programs.push(out.program(Y_SHADER)?);
            out.programs.push(out.program(UV_SHADER)?);
            let renderer = out
                .gl
                .as_ref()
                .unwrap()
                .get_parameter_string(glow::RENDERER);
            ensure!(
                !renderer.to_lowercase().contains("llvmpipe")
                    && !renderer.to_lowercase().contains("softpipe"),
                "拒绝软件 GLES renderer"
            );
            tracing::info!(renderer, "独立 EGL NV12 转换器已初始化");
            Ok(out)
        }
    }

    unsafe fn program(&self, fragment: &str) -> Result<glow::NativeProgram> {
        let gl = self.gl.as_ref().unwrap();
        let mut shaders = vec![];
        let result = (|| unsafe {
            for (kind, text) in [
                (
                    glow::VERTEX_SHADER,
                    "#version 300 es\nvoid main(){vec2 p=vec2(float((gl_VertexID<<1)&2),float(gl_VertexID&2));gl_Position=vec4(p*2.0-1.0,0,1);}",
                ),
                (glow::FRAGMENT_SHADER, fragment),
            ] {
                let s = gl.create_shader(kind).map_err(anyhow::Error::msg)?;
                shaders.push(s);
                gl.shader_source(s, text);
                gl.compile_shader(s);
                ensure!(
                    gl.get_shader_compile_status(s),
                    "{}",
                    gl.get_shader_info_log(s)
                );
            }
            let p = gl.create_program().map_err(anyhow::Error::msg)?;
            for s in &shaders {
                gl.attach_shader(p, *s);
            }
            gl.link_program(p);
            if !gl.get_program_link_status(p) {
                let log = gl.get_program_info_log(p);
                gl.delete_program(p);
                bail!("{log}");
            }
            Ok(p)
        })();
        for s in shaders {
            unsafe {
                gl.delete_shader(s);
            }
        }
        result
    }

    unsafe fn image_texture(
        &self,
        input: &RgbDma,
        views: &mut Vec<(Handle, glow::NativeTexture)>,
    ) -> Result<glow::NativeTexture> {
        let gl = self.gl.as_ref().unwrap();
        let attrs = [
            0x3057,
            input.width as i32,
            0x3056,
            input.height as i32,
            0x3271,
            input.fourcc as i32,
            0x3272,
            input.fd,
            0x3273,
            input.offset as i32,
            0x3274,
            input.pitch as i32,
            0x3443,
            input.modifier as i32,
            0x3444,
            (input.modifier >> 32) as i32,
            0x3038,
        ];
        unsafe {
            let image = (self.create_image)(
                self.display,
                std::ptr::null_mut(),
                0x3270,
                std::ptr::null_mut(),
                attrs.as_ptr(),
            );
            ensure!(!image.is_null(), "EGL DMA-BUF 导入失败");
            let tex = match gl.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    (self.destroy_image)(self.display, image);
                    bail!("{e}");
                }
            };
            views.push((image, tex));
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            (self.bind_image)(glow::TEXTURE_2D, image);
            ensure!(gl.get_error() == glow::NO_ERROR, "绑定 DMA 纹理失败");
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            Ok(tex)
        }
    }

    /// The source fd must remain dequeued and unchanged until this returns.
    ///
    /// `crop` selects the source region and `target` the output geometry, so a
    /// controller quality that maps to a smaller frame is scaled here instead of
    /// being read back to the CPU. At equal sizes the sampling is a texel
    /// centre, so the result matches the previous exact pass.
    pub fn convert(
        &self,
        rgb: &RgbDma,
        crop: (u32, u32, u32, u32),
        target: (u32, u32),
    ) -> Result<Arc<OwnedNv12>> {
        let (x, y, w, h) = crop;
        let (tw, th) = target;
        ensure!(
            w > 0
                && h > 0
                && w % 2 == 0
                && h % 2 == 0
                && x.checked_add(w).is_some_and(|n| n <= rgb.width)
                && y.checked_add(h).is_some_and(|n| n <= rgb.height),
            "NV12 crop 无效"
        );
        ensure!(
            tw > 0 && th > 0 && tw % 2 == 0 && th % 2 == 0,
            "NV12 目标尺寸无效"
        );
        let surface = self
            .va
            .create_surfaces(
                cros_libva::VA_RT_FORMAT_YUV420,
                Some(cros_libva::VA_FOURCC_NV12),
                tw,
                th,
                None,
                vec![()],
            )?
            .remove(0);
        let p = surface.export_prime_writable()?;
        ensure!(
            p.objects.len() == 1 && p.layers.len() == 1 && p.layers[0].num_planes == 2,
            "不支持多对象 NV12 输出"
        );
        let gl = self.gl.as_ref().unwrap();
        let mut views = vec![];
        // Clean up every temporary EGL/GL view on success and failure. Finish
        // also runs on errors before PipeWire is allowed to reuse the source.
        unsafe {
            let fbo = gl.create_framebuffer().map_err(anyhow::Error::msg)?;
            let result = (|| -> Result<()> {
                let source = self.image_texture(rgb, &mut views)?;
                let mut targets = vec![];
                for plane in 0..2 {
                    let obj = p.layers[0].object_index[plane] as usize;
                    ensure!(obj < p.objects.len(), "NV12 object 越界");
                    targets.push(self.image_texture(
                        &RgbDma {
                            fd: p.objects[obj].fd.as_raw_fd(),
                            width: tw >> plane,
                            height: th >> plane,
                            pitch: p.layers[0].pitch[plane],
                            offset: p.layers[0].offset[plane],
                            fourcc: if plane == 0 {
                                u32::from_le_bytes(*b"R8  ")
                            } else {
                                u32::from_le_bytes(*b"GR88")
                            },
                            modifier: p.objects[obj].drm_format_modifier,
                        },
                        &mut views,
                    )?);
                }
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(source));
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                for (plane, target) in targets.into_iter().enumerate() {
                    gl.framebuffer_texture_2d(
                        glow::FRAMEBUFFER,
                        glow::COLOR_ATTACHMENT0,
                        glow::TEXTURE_2D,
                        Some(target),
                        0,
                    );
                    ensure!(
                        gl.check_framebuffer_status(glow::FRAMEBUFFER)
                            == glow::FRAMEBUFFER_COMPLETE,
                        "NV12 平面不可渲染"
                    );
                    gl.viewport(0, 0, (tw >> plane) as i32, (th >> plane) as i32);
                    let prog = self.programs[plane];
                    gl.use_program(Some(prog));
                    gl.uniform_1_i32(gl.get_uniform_location(prog, "rgb").as_ref(), 0);
                    gl.uniform_2_i32(
                        gl.get_uniform_location(prog, "origin").as_ref(),
                        x as i32,
                        y as i32,
                    );
                    gl.uniform_2_i32(
                        gl.get_uniform_location(prog, "src").as_ref(),
                        w as i32,
                        h as i32,
                    );
                    gl.uniform_2_i32(
                        gl.get_uniform_location(prog, "dst").as_ref(),
                        tw as i32,
                        th as i32,
                    );
                    gl.draw_arrays(glow::TRIANGLES, 0, 3);
                }
                ensure!(gl.get_error() == glow::NO_ERROR, "NV12 GPU 转换失败");
                Ok(())
            })();
            gl.finish();
            let error = gl.get_error();
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_framebuffer(fbo);
            for (image, tex) in views {
                gl.delete_texture(tex);
                (self.destroy_image)(self.display, image);
            }
            result?;
            ensure!(error == glow::NO_ERROR, "NV12 GPU 同步失败");
        }
        Ok(Arc::new(OwnedNv12(p, self.prime2)))
    }
}
impl Drop for Converter {
    fn drop(&mut self) {
        unsafe {
            if let Some(gl) = &self.gl {
                gl.finish();
                for p in self.programs.drain(..) {
                    gl.delete_program(p);
                }
            }
            if !self.display.is_null() {
                (self.make_current)(
                    self.display,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                if !self.context.is_null() {
                    (self.destroy_context)(self.display, self.context);
                }
                (self.terminate)(self.display);
            }
            if !self.gbm.is_null() {
                (self.destroy_gbm)(self.gbm);
            }
        }
    }
}
