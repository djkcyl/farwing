//! Bounded optional readback. No plugin callback or pipe I/O runs on this thread.
use super::*;
use crate::plugins::{Sample, Shared};

struct Pending {
    sequence: u64,
    at: Instant,
    generation: u64,
}
struct Slot {
    texture: ID3D11Texture2D,
    pending: Option<Pending>,
}
pub(super) struct Capture {
    texture: ID3D11Texture2D,
    target: ID3D11RenderTargetView,
    renderer: VideoShaderRenderer,
    slots: Vec<Slot>,
    size: PhysicalSize<u32>,
    source: (u32, u32, u16),
    next: Instant,
    sequence: u64,
}
impl Capture {
    pub fn for_view(device: &ID3D11Device, view: &VideoTextureView) -> Result<Self> {
        let (w, h) = if matches!(view.rotation, 90 | 270) {
            (view.height, view.width)
        } else {
            (view.width, view.height)
        };
        let ratio = (640.0 / w.max(h).max(1) as f64).min(1.0);
        let size = PhysicalSize::new(
            (w as f64 * ratio).round().max(1.0) as u32,
            (h as f64 * ratio).round().max(1.0) as u32,
        );
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.width,
            Height: size.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
        let texture = texture.context("analysis target")?;
        let mut target = None;
        unsafe { device.CreateRenderTargetView(&texture, None, Some(&mut target)) }?;
        let mut slots = Vec::new();
        for _ in 0..2 {
            let desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..desc
            };
            let mut texture = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
            slots.push(Slot {
                texture: texture.context("analysis staging")?,
                pending: None,
            });
        }
        Ok(Self {
            texture,
            target: target.context("analysis RTV")?,
            renderer: VideoShaderRenderer::new(device)?,
            slots,
            size,
            source: (view.width, view.height, view.rotation),
            next: Instant::now(),
            sequence: 0,
        })
    }
    pub fn matches(&self, view: &VideoTextureView) -> bool {
        self.source == (view.width, view.height, view.rotation)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        view: VideoTextureView,
        state: &Shared,
        at: Instant,
        new_frame: bool,
    ) -> Result<()> {
        let generation = state.generation.load(Ordering::Acquire);
        for slot in &mut self.slots {
            if slot.pending.is_none() {
                continue;
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            match unsafe {
                context.Map(
                    &slot.texture,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            } {
                Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => continue,
                Err(error) => return Err(error.into()),
                Ok(()) => {}
            }
            let pending = slot.pending.take().expect("pending capture");
            if pending.generation == generation && pending.at.elapsed() < Duration::from_millis(250)
            {
                let stride = self.size.width as usize * 4;
                if mapped.pData.is_null() || (mapped.RowPitch as usize) < stride {
                    unsafe { context.Unmap(&slot.texture, 0) };
                    bail!("invalid analysis mapping");
                }
                let mut pixels = vec![0; stride * self.size.height as usize];
                for row in 0..self.size.height as usize {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                            pixels.as_mut_ptr().add(row * stride),
                            stride,
                        );
                    }
                }
                unsafe { context.Unmap(&slot.texture, 0) };
                state.submit(Sample {
                    motion: [0, 0],
                    generation: pending.generation,
                    sequence: pending.sequence,
                    width: self.size.width,
                    height: self.size.height,
                    at: pending.at,
                    pixels,
                });
            } else {
                unsafe { context.Unmap(&slot.texture, 0) };
            }
        }
        let now = Instant::now();
        if !new_frame || now < self.next {
            return Ok(());
        }
        let Some(slot) = self.slots.iter_mut().find(|s| s.pending.is_none()) else {
            return Ok(());
        };
        let interval = Duration::from_nanos(
            (1_000_000_000 / state.sample_fps.load(Ordering::Relaxed).max(1)).max(1),
        );
        // Advance the original sampling phase instead of accumulating render
        // jitter. Skip missed slots; never replay a backlog or duplicate frames.
        let remainder = now.duration_since(self.next).as_nanos() % interval.as_nanos();
        self.next = now + interval - Duration::from_nanos(remainder as u64);
        unsafe {
            context.ClearRenderTargetView(&self.target, &[0.0, 0.0, 0.0, 1.0]);
        }
        self.renderer
            .draw(device, context, &self.target, texture, view, self.size, 0)?;
        unsafe {
            context.OMSetRenderTargets(None, None);
            context.CopyResource(&slot.texture, &self.texture);
        }
        self.sequence = self.sequence.wrapping_add(1);
        slot.pending = Some(Pending {
            sequence: self.sequence,
            at,
            generation,
        });
        Ok(())
    }
}
