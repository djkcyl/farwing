//! Generic D3D11 executor. Pixel algorithms are supplied by external modules.
use crate::i18n::t;
use super::*;
use crate::plugins::{Graph, Tap};

const MAX_BYTES: usize = 512 * 1024 * 1024;
const MAX_QUEUE: usize = 16;
pub(super) struct Target {
    pub texture: ID3D11Texture2D,
    pub rtv: ID3D11RenderTargetView,
    pub srv: ID3D11ShaderResourceView,
    pub size: PhysicalSize<u32>,
}
#[derive(Default)]
struct Pool {
    targets: Vec<Arc<Target>>,
    bytes: usize,
}
impl Pool {
    fn get(&mut self, device: &ID3D11Device, size: PhysicalSize<u32>) -> Result<Arc<Target>> {
        if let Some(target) = self
            .targets
            .iter()
            .find(|t| t.size == size && Arc::strong_count(t) == 1)
        {
            return Ok(target.clone());
        }
        let bytes = (size.width as usize)
            .checked_mul(size.height as usize)
            .and_then(|n| n.checked_mul(8))
            .context("video texture overflow")?;
        anyhow::ensure!(
            size.width > 0
                && size.height > 0
                && size.width <= 8192
                && size.height <= 8192
                && self.targets.len() < 32
                && self.bytes + bytes <= MAX_BYTES,
            t().viewer.video_chain_texture_budget
        );
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.width,
            Height: size.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
        let texture = texture.context("effect texture")?;
        let mut rtv = None;
        unsafe { device.CreateRenderTargetView(&texture, None, Some(&mut rtv)) }?;
        let mut srv = None;
        unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut srv)) }?;
        let target = Arc::new(Target {
            texture,
            rtv: rtv.context("effect RTV")?,
            srv: srv.context("effect SRV")?,
            size,
        });
        self.targets.push(target.clone());
        self.bytes += bytes;
        Ok(target)
    }
}
#[derive(Clone, Copy)]
pub(super) struct Metadata {
    pub timing: RenderedFrameTiming,
    pub received_at: Instant,
}
#[derive(Clone)]
pub(super) struct Picture {
    pub target: Arc<Target>,
    pub at: Instant,
    pub generated: bool,
    pub metadata: Option<Metadata>,
}
struct Pass {
    spec: farwing_plugin_api::VideoPass,
    shader: ID3D11PixelShader,
    params: ID3D11Buffer,
    timing: ID3D11Buffer,
    history: Option<Picture>,
}
struct Node {
    id: u64,
    input: u64,
    passes: Vec<Pass>,
}
struct Scheduled {
    picture: Picture,
    due: Instant,
}
pub(super) struct Engine {
    nodes: Vec<Node>,
    output_source: u64,
    pub cache_revision: u64,
    normalizer: VideoShaderRenderer,
    renderer: VideoShaderRenderer,
    pool: Pool,
    queue: VecDeque<Scheduled>,
    last: Option<Picture>,
    delay: Duration,
    late_limit: Duration,
    signature: Option<(u32, u32, i32, RenderColor)>,
}
fn buffer(device: &ID3D11Device, data: &[f32; 4]) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        ..Default::default()
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr().cast(),
        ..Default::default()
    };
    let mut out = None;
    unsafe { device.CreateBuffer(&desc, Some(&init), Some(&mut out)) }?;
    out.context("effect constants")
}
pub(super) fn view(target: &Target, color: RenderColor) -> VideoTextureView {
    VideoTextureView {
        color,
        array_slice: 0,
        input_format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        visible_x: 0,
        visible_y: 0,
        coded_width: target.size.width,
        coded_height: target.size.height,
        width: target.size.width,
        height: target.size.height,
    }
}
impl Engine {
    pub fn new(device: &ID3D11Device, graph: &Graph, fps: f64) -> Result<Self> {
        let mut nodes = Vec::new();
        for node in &graph.nodes {
            let mut passes = Vec::new();
            for spec in &node.program.passes {
                let mut shader = None;
                unsafe { device.CreatePixelShader(&spec.shader, None, Some(&mut shader)) }?;
                passes.push(Pass {
                    spec: spec.clone(),
                    shader: shader.context("effect shader")?,
                    params: buffer(device, &spec.params)?,
                    timing: buffer(device, &[0.0; 4])?,
                    history: None,
                });
            }
            nodes.push(Node {
                id: node.id,
                input: node.input,
                passes,
            });
        }
        let mut delays = std::collections::BTreeMap::from([(0, 0u32)]);
        let mut rates = std::collections::BTreeMap::from([(0, 1usize)]);
        for node in &graph.nodes {
            delays.insert(node.id, delays[&node.input] + node.program.delay_frames);
            rates.insert(
                node.id,
                rates[&node.input]
                    * node
                        .program
                        .passes
                        .iter()
                        .map(|p| p.phases.len())
                        .product::<usize>(),
            );
        }
        let frames = *delays
            .get(&graph.output_source)
            .context(t().viewer.video_output_node_missing)?;
        let rate = rates[&graph.output_source];
        let fps = fps.clamp(15.0, 240.0);
        let delay = Duration::from_secs_f64(frames as f64 / fps);
        let late_limit = Duration::from_secs_f64(1.0 / (fps * rate as f64));
        Ok(Self {
            nodes,
            output_source: graph.output_source,
            cache_revision: 0,
            normalizer: VideoShaderRenderer::new(device)?,
            renderer: VideoShaderRenderer::new(device)?,
            pool: Pool::default(),
            queue: VecDeque::new(),
            last: None,
            delay,
            late_limit,
            signature: None,
        })
    }
    pub fn clear_history(&mut self) {
        self.cache_revision = self.cache_revision.wrapping_add(1);
        for node in &mut self.nodes {
            for pass in &mut node.passes {
                pass.history = None;
            }
        }
        self.queue.clear();
        self.last = None;
        self.pool = Pool::default();
        self.normalizer.reset_input_cache();
        self.renderer.reset_input_cache();
    }
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        input: VideoTextureView,
        at: Instant,
        metadata: Option<Metadata>,
        captures: &mut Captures,
        taps: &[Tap],
    ) -> Result<()> {
        let signature = (
            input.width,
            input.height,
            input.input_format.0,
            input.color,
        );
        if self.signature != Some(signature) {
            self.clear_history();
            self.signature = Some(signature);
            for tap in taps {
                if tap.source != 0 {
                    tap.state.generation.fetch_add(1, Ordering::AcqRel);
                }
            }
        }
        let size = PhysicalSize::new(input.width, input.height);
        let normalized = self.pool.get(device, size)?;
        self.normalizer
            .draw(device, context, &normalized.rtv, texture, input, size, 0)?;
        let mut frames = std::collections::BTreeMap::from([(
            0,
            vec![Picture {
                target: normalized,
                at,
                generated: false,
                metadata,
            }],
        )]);
        for node in &mut self.nodes {
            let mut pictures = frames
                .get(&node.input)
                .context(t().viewer.video_node_input_not_ready)?
                .clone();
            for pass in &mut node.passes {
                let mut output = Vec::new();
                for picture in pictures {
                    let history = pass.history.as_ref().filter(|p| {
                        p.target.size == picture.target.size
                            && picture.at > p.at
                            && picture.at.duration_since(p.at) < Duration::from_millis(250)
                    });
                    let phases = if history.is_some() {
                        pass.spec.phases.as_slice()
                    } else {
                        &[1.0]
                    };
                    for phase in phases {
                        let numerator = pass.spec.scale_numerator as u64;
                        let denominator = pass.spec.scale_denominator as u64;
                        let size = PhysicalSize::new(
                            ((picture.target.size.width as u64 * numerator) / denominator)
                                .max(1)
                                .min(u32::MAX as u64) as u32,
                            ((picture.target.size.height as u64 * numerator) / denominator)
                                .max(1)
                                .min(u32::MAX as u64) as u32,
                        );
                        let target = self.pool.get(device, size)?;
                        let previous = history.map_or(&picture.target, |h| &h.target);
                        let previous_view = Some(previous.srv.clone());
                        let timing = [
                            *phase,
                            picture.target.size.width as f32,
                            picture.target.size.height as f32,
                            if history.is_some() { 1.0 } else { 0.0 },
                        ];
                        unsafe {
                            context.OMSetRenderTargets(None, None);
                            context.UpdateSubresource(
                                &pass.timing,
                                0,
                                None,
                                timing.as_ptr().cast(),
                                0,
                                0,
                            );
                            context.PSSetConstantBuffers(
                                1,
                                Some(&[Some(pass.timing.clone()), Some(pass.params.clone())]),
                            );
                            context.PSSetShaderResources(2, Some(&[previous_view]));
                        }
                        self.renderer.set_rgba_shader(pass.shader.clone());
                        let result = self.renderer.draw(
                            device,
                            context,
                            &target.rtv,
                            &picture.target.texture,
                            view(&picture.target, input.color),
                            size,
                            0,
                        );
                        unsafe {
                            context.PSSetShaderResources(2, Some(&[None]));
                            context.PSSetConstantBuffers(1, Some(&[None, None]));
                        }
                        result?;
                        let generated = picture.generated || *phase < 1.0;
                        let time = if *phase < 1.0 {
                            let h = history.context("temporal output without history")?;
                            h.at + picture.at.duration_since(h.at).mul_f32(*phase)
                        } else {
                            picture.at
                        };
                        output.push(Picture {
                            target,
                            at: time,
                            generated,
                            metadata: if generated { None } else { picture.metadata },
                        });
                    }
                    if pass.spec.phases.len() > 1 {
                        pass.history = Some(picture);
                    }
                }
                pictures = output;
            }
            if let Some(real) = pictures.iter().rev().find(|p| !p.generated) {
                captures.feed(
                    node.id,
                    device,
                    context,
                    &real.target.texture,
                    view(&real.target, input.color),
                    taps,
                    real.at,
                    true,
                );
            }
            frames.insert(node.id, pictures);
        }
        let pictures = frames
            .remove(&self.output_source)
            .context(t().viewer.video_output_no_picture)?;
        anyhow::ensure!(
            self.queue.len() + pictures.len() <= MAX_QUEUE,
            t().viewer.video_chain_queue_full
        );
        for picture in pictures {
            let due = picture.at + self.delay;
            self.queue.push_back(Scheduled { picture, due });
        }
        self.queue.make_contiguous().sort_by_key(|s| s.due);
        Ok(())
    }
    pub fn next_due(&self) -> Option<Instant> {
        self.queue.front().map(|s| s.due)
    }
    pub fn take_due(&mut self, skipped: &std::sync::atomic::AtomicU64) -> Option<Picture> {
        let now = Instant::now();
        while let Some(front) = self.queue.front() {
            if front.due > now {
                return None;
            }
            let next = self.queue.pop_front().expect("scheduled picture");
            if next.picture.generated && now.saturating_duration_since(next.due) > self.late_limit {
                skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            self.last = Some(next.picture.clone());
            return Some(next.picture);
        }
        None
    }
    pub fn last(&self) -> Option<Picture> {
        self.last.clone()
    }
}

#[derive(Default)]
pub(super) struct Captures {
    entries: Vec<(u64, u64, u64, plugin_capture::Capture)>,
}
impl Captures {
    #[allow(clippy::too_many_arguments)]
    pub fn feed(
        &mut self,
        source: u64,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        view: VideoTextureView,
        taps: &[Tap],
        at: Instant,
        new_frame: bool,
    ) {
        self.entries.retain(|(id, src, revision, _)| {
            taps.iter().any(|tap| {
                tap.id == *id
                    && tap.source == *src
                    && tap.revision == *revision
                    && tap.state.enabled.load(Ordering::Acquire)
            })
        });
        for tap in taps.iter().filter(|t| {
            t.source == source
                && t.state.enabled.load(Ordering::Acquire)
                && t.state.ready.load(Ordering::Acquire)
        }) {
            let result = (|| -> Result<()> {
                let index = if let Some(index) =
                    self.entries.iter().position(|(id, _, _, _)| *id == tap.id)
                {
                    index
                } else {
                    self.entries.push((
                        tap.id,
                        source,
                        tap.revision,
                        plugin_capture::Capture::for_view(device, &view)?,
                    ));
                    self.entries.len() - 1
                };
                if !self.entries[index].3.matches(&view) {
                    self.entries[index].3 = plugin_capture::Capture::for_view(device, &view)?;
                    tap.state.generation.fetch_add(1, Ordering::AcqRel);
                }
                self.entries[index]
                    .3
                    .tick(device, context, texture, view, &tap.state, at, new_frame)
            })();
            if let Err(error) = result {
                tap.state
                    .fail((t().viewer.frame_capture_failed)(&error.to_string()));
            }
        }
    }
}

fn half_to_f32(half: u16) -> f32 {
    let sign = if half & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((half >> 10) & 0x1f);
    let mantissa = f32::from(half & 0x3ff);
    sign * match exponent {
        0 => mantissa * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + mantissa / 1024.0) * 2f32.powi(exponent - 15),
    }
}

/// `dev plugins`: run `graph` once on an RGBA8 picture with the real executor
/// on a hardware device, and read the output back (linear RGBA floats).
pub(crate) fn dev_render(graph: &Graph, pixels: &[[u8; 4]], width: u32, height: u32) -> Result<Vec<[f32; 4]>> {
    anyhow::ensure!(pixels.len() == (width * height) as usize, "picture size");
    unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device: ID3D11Device = device.context("D3D11 device")?;
        let context: ID3D11DeviceContext = context.context("D3D11 context")?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: width * 4,
            ..Default::default()
        };
        let mut input = None;
        device.CreateTexture2D(&desc, Some(&data), Some(&mut input))?;
        let input = input.context("input texture")?;
        let mut engine = Engine::new(&device, graph, 60.0)?;
        let view = VideoTextureView {
            color: RenderColor::default(),
            array_slice: 0,
            input_format: DXGI_FORMAT_R8G8B8A8_UNORM,
            visible_x: 0,
            visible_y: 0,
            coded_width: width,
            coded_height: height,
            width,
            height,
        };
        engine.process(
            &device,
            &context,
            &input,
            view,
            Instant::now() - Duration::from_secs(1),
            None,
            &mut Captures::default(),
            &[],
        )?;
        let picture = engine
            .take_due(&std::sync::atomic::AtomicU64::new(0))
            .context("the graph produced no picture")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        picture.target.texture.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.context("staging texture")?;
        context.CopyResource(&staging, &picture.target.texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let mut out = Vec::with_capacity(pixels.len());
        for y in 0..desc.Height as usize {
            let row = mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize).cast::<u16>();
            for x in 0..desc.Width as usize {
                let p = std::slice::from_raw_parts(row.add(x * 4), 4);
                out.push([half_to_f32(p[0]), half_to_f32(p[1]), half_to_f32(p[2]), half_to_f32(p[3])]);
            }
        }
        context.Unmap(&staging, 0);
        Ok(out)
    }
}
