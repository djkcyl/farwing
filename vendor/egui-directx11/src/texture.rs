// This file contains implementations inspired by or derived from the following
// sources:
// - https://github.com/ohchase/egui-directx/blob/master/egui-directx11/src/texture.rs
//
// Here I would express my gratitude for their contributions to the Rust
// community. Their work served as a valuable reference and inspiration for this
// project.
//
// Nekomaru, March 2024

use std::{collections::HashMap, mem};

use egui::{
    Color32, ImageData, TextureFilter, TextureId, TextureOptions, TextureWrapMode, TexturesDelta,
};

use windows::{
    Win32::Graphics::{Direct3D11::*, Dxgi::Common::*},
    core::Result,
};

struct ManagedTexture {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    pixels: Vec<Color32>,
    width: usize,
    sampler: ID3D11SamplerState,
}

enum Texture {
    /// A texture managed by egui (created from ImageData)
    Managed(ManagedTexture),
    /// A user-provided texture (registered from an existing shader resource view)
    User { srv: ID3D11ShaderResourceView },
}
impl Texture {
    pub fn is_managed(&self) -> bool {
        matches!(self, Texture::Managed(_))
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Texture::User { .. })
    }
}

pub struct TexturePool {
    device: ID3D11Device,
    pool: HashMap<TextureId, Texture>,
    next_user_texture_id: u64,
    samplers: HashMap<TextureOptions, ID3D11SamplerState>,
    user_sampler: ID3D11SamplerState,
}

impl TexturePool {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let user_sampler = create_sampler(device, TextureOptions::LINEAR)?;
        Ok(Self {
            device: device.clone(),
            pool: HashMap::new(),
            next_user_texture_id: 0,
            samplers: HashMap::from([(TextureOptions::LINEAR, user_sampler.clone())]),
            user_sampler,
        })
    }

    pub fn binding(
        &self,
        tid: TextureId,
    ) -> Option<(ID3D11ShaderResourceView, ID3D11SamplerState)> {
        self.pool.get(&tid).map(|t| match t {
            Texture::Managed(managed) => (managed.srv.clone(), managed.sampler.clone()),
            Texture::User { srv } => (srv.clone(), self.user_sampler.clone()),
        })
    }

    fn sampler(&mut self, options: TextureOptions) -> Result<ID3D11SamplerState> {
        if let Some(sampler) = self.samplers.get(&options) {
            return Ok(sampler.clone());
        }
        let sampler = create_sampler(&self.device, options)?;
        self.samplers.insert(options, sampler.clone());
        Ok(sampler)
    }

    /// Register a user-provided shader resource view and get a TextureId for it.
    /// This TextureId can be used in egui to reference this texture.
    ///
    /// The returned TextureId will be unique and won't conflict with egui's managed textures.
    pub fn register_user_texture(&mut self, srv: ID3D11ShaderResourceView) -> TextureId {
        let id = TextureId::User(self.next_user_texture_id);
        self.next_user_texture_id += 1;
        self.pool.insert(id, Texture::User { srv });
        id
    }

    /// Unregister a user texture by its TextureId.
    /// Returns true if the texture was found and removed, false otherwise.
    pub fn unregister_user_texture(&mut self, tid: TextureId) -> bool {
        if self.pool.get(&tid).is_some_and(|t| t.is_user()) {
            self.pool.remove(&tid);
            true
        } else {
            false
        }
    }

    pub fn update(&mut self, ctx: &ID3D11DeviceContext, mut delta: TexturesDelta) -> Result<()> {
        for (tid, deltas) in delta.set.drain() {
            for delta in deltas {
                let sampler = self.sampler(delta.options)?;
                if delta.is_whole() && delta.image.width() > 0 && delta.image.height() > 0 {
                    self.pool.insert(
                        tid,
                        Self::create_managed_texture(&self.device, delta.image, sampler)?,
                    );
                    // the old texture is returned and dropped here, freeing
                    // all its gpu resource.
                } else if let Some(tex) = self.pool.get_mut(&tid).filter(|t| t.is_managed()) {
                    if let Some(pos) = delta.pos {
                        Self::update_partial(ctx, tex, delta.image, pos, sampler)?;
                    }
                } else {
                    log::warn!(
                        "egui wants to update a non-existing texture {tid:?}. this request will be ignored."
                    );
                }
            }
        }
        for tid in delta.free.drain() {
            if self.pool.get(&tid).is_some_and(|t| t.is_managed()) {
                self.pool.remove(&tid);
            }
        }
        Ok(())
    }

    fn update_partial(
        ctx: &ID3D11DeviceContext,
        old: &mut Texture,
        image: ImageData,
        [nx, ny]: [usize; 2],
        sampler: ID3D11SamplerState,
    ) -> Result<()> {
        let Texture::Managed(old) = old else {
            log::warn!("attempted to partially update a user texture, which is not supported");
            return Ok(());
        };

        let ImageData::Color(image) = image;
        let height = old.pixels.len() / old.width;
        if nx.checked_add(image.width()).is_none_or(|x| x > old.width)
            || ny.checked_add(image.height()).is_none_or(|y| y > height)
            || image.width().checked_mul(image.height()) != Some(image.pixels.len())
        {
            return Err(windows::core::Error::new(
                windows::core::HRESULT(0x80070057u32 as i32),
                "partial texture update exceeds its image",
            ));
        }
        let row_bytes = old.width * mem::size_of::<Color32>();
        let subr = unsafe {
            let mut output = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&old.tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut output))?;
            output
        };
        if subr.pData.is_null() || (subr.RowPitch as usize) < row_bytes {
            unsafe { ctx.Unmap(&old.tex, 0) };
            return Err(windows::core::Error::new(
                windows::core::HRESULT(0x80070057u32 as i32),
                "invalid mapped GUI texture pitch",
            ));
        }
        for y in 0..image.height() {
            let whole = (ny + y) * old.width + nx;
            let part = y * image.width();
            old.pixels[whole..whole + image.width()]
                .copy_from_slice(&image.pixels[part..part + image.width()]);
        }
        // WRITE_DISCARD invalidates the whole mapped image. Restore every row,
        // respecting driver padding (small QR textures rarely have a tight pitch).
        for y in 0..height {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    old.pixels.as_ptr().add(y * old.width).cast::<u8>(),
                    subr.pData.cast::<u8>().add(y * subr.RowPitch as usize),
                    row_bytes,
                );
            }
        }
        unsafe { ctx.Unmap(&old.tex, 0) };
        old.sampler = sampler;
        Ok(())
    }

    fn create_managed_texture(
        device: &ID3D11Device,
        data: ImageData,
        sampler: ID3D11SamplerState,
    ) -> Result<Texture> {
        let width = data.width();

        let pixels = match &data {
            ImageData::Color(c) => c.pixels.clone(),
        };

        let desc = D3D11_TEXTURE2D_DESC {
            Width: data.width() as _,
            Height: data.height() as _,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as _,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as _,
            ..Default::default()
        };

        let subresource_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as _,
            SysMemPitch: (width * mem::size_of::<Color32>()) as u32,
            SysMemSlicePitch: 0,
        };

        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc, Some(&subresource_data), Some(&mut tex)) }?;
        let tex = tex.unwrap();

        let mut srv = None;
        unsafe { device.CreateShaderResourceView(&tex, None, Some(&mut srv)) }?;
        let srv = srv.unwrap();

        Ok(Texture::Managed(ManagedTexture {
            tex,
            srv,
            width,
            pixels,
            sampler,
        }))
    }
}

fn create_sampler(device: &ID3D11Device, options: TextureOptions) -> Result<ID3D11SamplerState> {
    let linear = |filter| i32::from(filter == TextureFilter::Linear);
    let filter = D3D11_FILTER(
        (linear(options.minification) << 4)
            | (linear(options.magnification) << 2)
            | linear(options.mipmap_mode.unwrap_or(TextureFilter::Nearest)),
    );
    let address = match options.wrap_mode {
        TextureWrapMode::ClampToEdge => D3D11_TEXTURE_ADDRESS_CLAMP,
        TextureWrapMode::Repeat => D3D11_TEXTURE_ADDRESS_WRAP,
        TextureWrapMode::MirroredRepeat => D3D11_TEXTURE_ADDRESS_MIRROR,
    };
    let desc = D3D11_SAMPLER_DESC {
        Filter: filter,
        AddressU: address,
        AddressV: address,
        AddressW: address,
        ComparisonFunc: D3D11_COMPARISON_ALWAYS,
        MaxAnisotropy: 1,
        // Managed GUI textures have one mip level.
        MinLOD: 0.0,
        MaxLOD: 0.0,
        ..Default::default()
    };
    let mut sampler = None;
    unsafe { device.CreateSamplerState(&desc, Some(&mut sampler)) }?;
    Ok(sampler.unwrap())
}
