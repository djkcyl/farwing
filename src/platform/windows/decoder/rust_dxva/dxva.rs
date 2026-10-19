// SPDX-License-Identifier: LGPL-2.1-or-later
//! Pure Rust D3D11 decode submission and owning texture leases.
use anyhow::{Context, Result, bail, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::{
    Win32::Graphics::{
        Direct3D11::*,
        Dxgi::{Common::*, *},
    },
    core::{GUID, HRESULT, Interface},
};
#[derive(Clone, Copy, Debug)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}
#[derive(Debug)]
pub enum Failure {
    Unsupported,
    HardwareFailure,
    OutputBusy,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "unsupported DXVA stream configuration",
            Self::HardwareFailure => "DXVA device or surface operation failed",
            Self::OutputBusy => "DXVA display surface temporarily busy",
        })
    }
}
impl std::error::Error for Failure {}
fn select(
    video: &ID3D11VideoDevice,
    codec: Codec,
    width: u32,
    height: u32,
    depth: u8,
    chroma: u8,
) -> Result<(D3D11_VIDEO_DECODER_DESC, D3D11_VIDEO_DECODER_CONFIG)> {
    ensure!(
        width > 0 && height > 0 && width <= 16384 && height <= 16384,
        "invalid decoder geometry"
    );
    // Range-extension GUIDs from the Windows SDK d3d11.h (not in windows-rs).
    let guid = match (codec, depth, chroma) {
        (Codec::H264, 8, 1) => D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
        (Codec::Hevc, 8, 1) => D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
        (Codec::Hevc, 10, 1) => D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10,
        (Codec::Av1, 8 | 10, 1) => GUID::from_u128(0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a),
        (Codec::Av1, 8 | 10, 3) => GUID::from_u128(0x6936ff0f_45b1_4163_9cc1_646ef6946108),
        (Codec::Hevc, 8, 3) => GUID::from_u128(0x4008018f_f537_4b36_98cf_61af8a2c1a33),
        (Codec::Hevc, 10, 3) => GUID::from_u128(0x0dabeffa_4458_4602_bc03_0795659d617c),
        _ => bail!("unsupported codec/depth"),
    };
    let format = if chroma == 3 {
        if depth == 10 {
            DXGI_FORMAT_Y410
        } else {
            DXGI_FORMAT_AYUV
        }
    } else if depth == 10 {
        DXGI_FORMAT_P010
    } else {
        DXGI_FORMAT_NV12
    };
    unsafe {
        let mut found = false;
        for i in 0..video.GetVideoDecoderProfileCount() {
            if video.GetVideoDecoderProfile(i)? == guid {
                found = true;
                break;
            }
        }
        ensure!(
            found && video.CheckVideoDecoderFormat(&guid, format)?.as_bool(),
            "unsupported DXVA profile/format"
        );
        let desc = D3D11_VIDEO_DECODER_DESC {
            Guid: guid,
            SampleWidth: width,
            SampleHeight: height,
            OutputFormat: format,
        };
        let raw = if matches!(codec, Codec::H264) { 2 } else { 1 };
        let no_encrypt = GUID::from_u128(0x1b81_bed0_a0c7_11d3_b984_00c0_4f2e_73c5);
        let mut best = None;
        for i in 0..video.GetVideoDecoderConfigCount(&desc)? {
            let mut cfg = D3D11_VIDEO_DECODER_CONFIG::default();
            video.GetVideoDecoderConfig(&desc, i, &mut cfg)?;
            if cfg.ConfigBitstreamRaw != raw {
                continue;
            }
            let score = raw
                + 16 * u32::from(cfg.guidConfigBitstreamEncryption == no_encrypt)
                + 32 * u32::from(cfg.ConfigDecoderSpecific & 0x4000 != 0);
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, cfg));
            }
        }
        Ok((desc, best.context("no supported DXVA configuration")?.1))
    }
}
struct Texture {
    texture: ID3D11Texture2D,
    view: Option<ID3D11VideoDecoderOutputView>,
    sync: Option<IDXGIKeyedMutex>,
    busy: AtomicBool,
}
pub struct Pool {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    video: ID3D11VideoContext,
    multithread: ID3D11Multithread,
    decoder: ID3D11VideoDecoder,
    decode: Vec<Texture>,
    display: Vec<Texture>,
    single: bool,
    shared: bool,
    pub width: u32,
    pub height: u32,
    pub depth: u8,
    pub chroma: u8,
    pub dpb: usize,
}
/// Owning decoded surface and its visible rectangle; cropping never moves or
/// rewrites reference pixels. Presentation order remains the caller's job.
#[derive(Clone)]
pub struct Picture {
    pub surface: Arc<Lease>,
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    pub poc: i32,
    pub reorder_limit: u32,
    /// A new coded sequence: true discards prior output, false drains it.
    pub sequence_start: Option<bool>,
    pub needed_for_output: bool,
}
unsafe impl Send for Pool {}
unsafe impl Sync for Pool {}
pub struct Lease {
    pub pool: Arc<Pool>,
    pub index: usize,
    output: bool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.item().busy.store(false, Ordering::Release);
    }
}
impl Lease {
    fn item(&self) -> &Texture {
        if self.output {
            &self.pool.display[self.index]
        } else {
            &self.pool.decode[self.index]
        }
    }
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.item().texture
    }
    pub fn subresource(&self) -> u32 {
        if self.output || self.pool.single {
            0
        } else {
            self.index as u32
        }
    }
}
struct ThreadGuard<'a>(&'a ID3D11Multithread);
impl Drop for ThreadGuard<'_> {
    fn drop(&mut self) {
        unsafe { self.0.Leave() }
    }
}
struct SyncGuard(Option<IDXGIKeyedMutex>);
impl SyncGuard {
    fn acquire(sync: &Option<IDXGIKeyedMutex>) -> Result<Self> {
        Self::acquire_for(sync, 100)
    }
    fn acquire_for(sync: &Option<IDXGIKeyedMutex>, timeout: u32) -> Result<Self> {
        if let Some(s) = sync {
            let hr = unsafe { (s.vtable().AcquireSync)(s.as_raw(), 0, timeout) };
            if timeout == 0 && hr.0 == windows::Win32::Foundation::WAIT_TIMEOUT.0 as i32 {
                return Err(Failure::OutputBusy.into());
            }
            ensure!(hr == HRESULT(0), "keyed mutex unavailable: {hr:?}");
        }
        Ok(Self(sync.clone()))
    }
    fn release(mut self) -> Result<()> {
        if let Some(sync) = self.0.take() {
            unsafe { sync.ReleaseSync(0)? }
        }
        Ok(())
    }
}
impl Drop for SyncGuard {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            let _ = unsafe { s.ReleaseSync(0) };
        }
    }
}
struct Buffer<'a> {
    pool: &'a Pool,
    kind: D3D11_VIDEO_DECODER_BUFFER_TYPE,
    data: *mut u8,
    len: usize,
    released: bool,
}
impl<'a> Buffer<'a> {
    fn new(pool: &'a Pool, kind: D3D11_VIDEO_DECODER_BUFFER_TYPE) -> Result<Self> {
        let mut size = 0;
        let mut ptr = std::ptr::null_mut();
        unsafe {
            pool.video
                .GetDecoderBuffer(&pool.decoder, kind, &mut size, &mut ptr)?;
        }
        let buffer = Self {
            pool,
            kind,
            data: ptr.cast(),
            len: size as usize,
            released: false,
        };
        ensure!(!buffer.data.is_null(), "null decoder buffer");
        Ok(buffer)
    }
    fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.data, self.len) }
    }
    fn release(mut self) -> Result<()> {
        self.released = true;
        unsafe {
            self.pool
                .video
                .ReleaseDecoderBuffer(&self.pool.decoder, self.kind)?;
        }
        Ok(())
    }
}
impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = unsafe {
                self.pool
                    .video
                    .ReleaseDecoderBuffer(&self.pool.decoder, self.kind)
            };
        }
    }
}
impl Pool {
    pub fn probe(
        device: &ID3D11Device,
        codec: Codec,
        w: u32,
        h: u32,
        depth: u8,
        chroma: u8,
    ) -> Result<()> {
        let video: ID3D11VideoDevice = device.cast()?;
        let (d, c) = select(&video, codec, w, h, depth, chroma)?;
        let _decoder = unsafe { video.CreateVideoDecoder(&d, &c)? };
        Ok(())
    }
    pub fn new(
        device: ID3D11Device,
        codec: Codec,
        width: u32,
        height: u32,
        depth: u8,
        chroma: u8,
        dpb: usize,
    ) -> Result<Arc<Self>> {
        ensure!((1..=16).contains(&dpb), "invalid DPB size");
        let context = unsafe { device.GetImmediateContext()? };
        let vd: ID3D11VideoDevice = device.cast()?;
        let video: ID3D11VideoContext = context.cast()?;
        let multithread: ID3D11Multithread = context.cast()?;
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
        let (desc, cfg) = select(&vd, codec, width, height, depth, chroma)?;
        let decoder = unsafe { vd.CreateVideoDecoder(&desc, &cfg)? };
        let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS::default();
        unsafe {
            device.CheckFeatureSupport(
                D3D11_FEATURE_D3D11_OPTIONS,
                (&mut options as *mut D3D11_FEATURE_DATA_D3D11_OPTIONS).cast(),
                std::mem::size_of_val(&options) as u32,
            )?;
        }
        ensure!(
            options.ExtendedResourceSharing.as_bool(),
            "extended resource sharing unavailable"
        );
        let mut shared = true;
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi.GetAdapter()? };
        let adapter_desc = unsafe { adapter.GetDesc()? };
        if adapter_desc.VendorId == 0x1002
            && matches!(codec, Codec::H264)
            && let Ok(version) = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
            && (version as u64 >> 48) < 23
        {
            shared = false;
        }
        let single = cfg.ConfigDecoderSpecific & 0x4000 != 0;
        let count = (dpb + 5).max(cfg.ConfigMinRenderTargetBuffCount as usize);
        ensure!(
            count <= 127,
            "driver requires too many indexed decode surfaces"
        );
        let mut decode = Vec::<Texture>::new();
        let mut display = vec![];
        let mut td = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: if single { 1 } else { count as u32 },
            Format: desc.OutputFormat,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_DECODER.0
                | if single {
                    D3D11_BIND_SHADER_RESOURCE.0
                } else {
                    0
                }) as u32,
            MiscFlags: if single && shared {
                D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32
            } else {
                0
            },
            ..Default::default()
        };
        for i in 0..count {
            let texture = if i > 0 && !single {
                decode[0].texture.clone()
            } else {
                create_texture(&device, &td)?
            };
            let view_desc = D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC {
                DecodeProfile: desc.Guid,
                ViewDimension: D3D11_VDOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VDOV {
                        ArraySlice: if single { 0 } else { i as u32 },
                    },
                },
            };
            let mut view = None;
            unsafe {
                vd.CreateVideoDecoderOutputView(&texture, &view_desc, Some(&mut view))?;
            }
            let sync = if single && shared {
                Some(texture.cast()?)
            } else {
                None
            };
            decode.push(Texture {
                texture,
                view: Some(view.context("missing decoder view")?),
                sync,
                busy: AtomicBool::new(false),
            });
        }
        td.ArraySize = 1;
        td.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
        td.MiscFlags = if shared {
            D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32
        } else {
            0
        };
        if !single {
            for _ in 0..count.clamp(6, 8) {
                let texture = create_texture(&device, &td)?;
                let sync = if shared { Some(texture.cast()?) } else { None };
                display.push(Texture {
                    texture,
                    view: None,
                    sync,
                    busy: AtomicBool::new(false),
                });
            }
        }
        Ok(Arc::new(Self {
            _device: device,
            context,
            video,
            multithread,
            decoder,
            decode,
            display,
            single,
            shared,
            width,
            height,
            depth,
            chroma,
            dpb,
        }))
    }
    pub fn lease(self: &Arc<Self>, output: bool) -> Result<Lease> {
        let list = if output { &self.display } else { &self.decode };
        for (i, t) in list.iter().enumerate() {
            if t.busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(Lease {
                    pool: Arc::clone(self),
                    index: i,
                    output,
                });
            }
        }
        if output {
            return Err(Failure::OutputBusy.into());
        }
        bail!("DXVA texture pool exhausted")
    }
    fn lock(&self) -> ThreadGuard<'_> {
        unsafe {
            self.multithread.Enter();
        }
        ThreadGuard(&self.multithread)
    }
    pub fn display(self: &Arc<Self>, source: Arc<Lease>) -> Result<Arc<Lease>> {
        if self.single {
            return Ok(source);
        }
        let target = self.lease(true)?;
        let sync = SyncGuard::acquire_for(&target.item().sync, 0)?;
        {
            let _lock = self.lock();
            unsafe {
                self.context.CopySubresourceRegion(
                    target.texture(),
                    0,
                    0,
                    0,
                    0,
                    source.texture(),
                    source.subresource(),
                    None,
                );
                if self.shared {
                    self.context.Flush();
                }
            }
        }
        sync.release()?;
        Ok(Arc::new(target))
    }
    fn commit(
        &self,
        kind: D3D11_VIDEO_DECODER_BUFFER_TYPE,
        data: &[u8],
    ) -> Result<D3D11_VIDEO_DECODER_BUFFER_DESC> {
        let mut buffer = Buffer::new(self, kind)?;
        ensure!(
            buffer.len >= data.len(),
            "decoder parameter buffer too small"
        );
        buffer.bytes()[..data.len()].copy_from_slice(data);
        buffer.release()?;
        Ok(D3D11_VIDEO_DECODER_BUFFER_DESC {
            BufferType: kind,
            DataSize: data.len() as u32,
            ..Default::default()
        })
    }
    pub fn submit(
        &self,
        current: &Lease,
        picture: &[u8],
        matrix: &[u8],
        slices: &[&[u8]],
        cancel: &AtomicBool,
    ) -> Result<()> {
        ensure!(!slices.is_empty(), "empty picture");
        self.submit_with(current, cancel, || {
            self.submit_buffers(picture, matrix, slices, cancel)
        })
    }
    pub fn submit_av1(
        &self,
        current: &Lease,
        picture: &[u8],
        tiles: &[u8],
        bitstream: &[u8],
        cancel: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            !tiles.is_empty() && !bitstream.is_empty(),
            "empty AV1 picture"
        );
        self.submit_with(current, cancel, || {
            let mut padded = bitstream.to_vec();
            padded.resize(padded.len().div_ceil(128) * 128, 0);
            let desc = [
                self.commit(D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, picture)?,
                self.commit(D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, tiles)?,
                self.commit(D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, &padded)?,
            ];
            unsafe {
                self.video.SubmitDecoderBuffers(&self.decoder, &desc)?;
            }
            Ok(())
        })
    }
    fn submit_with(
        &self,
        current: &Lease,
        cancel: &AtomicBool,
        submit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let sync = SyncGuard::acquire(&current.item().sync)?;
        let mut began = false;
        let mut guard = None;
        for attempt in 0..200 {
            ensure!(!cancel.load(Ordering::Acquire), "decode cancelled");
            let lock = self.lock();
            match unsafe {
                self.video.DecoderBeginFrame(
                    &self.decoder,
                    current.item().view.as_ref().context("missing view")?,
                    0,
                    None,
                )
            } {
                Ok(()) => {
                    guard = Some(lock);
                    began = true;
                    break;
                }
                Err(e) => {
                    drop(lock);
                    if e.code() != HRESULT(0x8000000au32 as i32)
                        && e.code() != HRESULT(0x8876021cu32 as i32)
                    {
                        return Err(e.into());
                    }
                    if attempt < 10 {
                        std::thread::yield_now()
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
            }
        }
        ensure!(began, "DecoderBeginFrame busy timeout");
        let submitted = submit();
        let ended = unsafe { self.video.DecoderEndFrame(&self.decoder) };
        if self.shared {
            unsafe {
                self.context.Flush();
            }
        }
        drop(guard);
        let released = sync.release();
        submitted?;
        ended?;
        released?;
        Ok(())
    }
    fn submit_buffers(
        &self,
        picture: &[u8],
        matrix: &[u8],
        slices: &[&[u8]],
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut slice = 0;
        let mut offset = 0;
        let mut first = true;
        while slice < slices.len() {
            ensure!(!cancel.load(Ordering::Acquire), "decode cancelled");
            let mut desc = vec![];
            if first {
                desc.push(self.commit(D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, picture)?);
                if !matrix.is_empty() {
                    desc.push(self.commit(
                        D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX,
                        matrix,
                    )?);
                }
            }
            let mut bits = Buffer::new(self, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM)?;
            let mut control = Buffer::new(self, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL)?;
            let capacity = bits.len & !127;
            ensure!(
                capacity >= 128 && control.len >= 10,
                "invalid DXVA buffer capacity"
            );
            let mut written = 0;
            let mut n = 0;
            let mut last_size = 0;
            while slice < slices.len() && n < control.len / 10 {
                let prefix = if offset == 0 { 3 } else { 0 };
                if capacity - written <= prefix {
                    break;
                }
                let take = (slices[slice].len() - offset).min(capacity - written - prefix);
                let chopped = (if offset > 0 { 2u16 } else { 0 })
                    | (if offset + take < slices[slice].len() {
                        1
                    } else {
                        0
                    });
                let loc = n * 10;
                control.bytes()[loc..loc + 4].copy_from_slice(&(written as u32).to_le_bytes());
                last_size = prefix + take;
                control.bytes()[loc + 4..loc + 8]
                    .copy_from_slice(&(last_size as u32).to_le_bytes());
                control.bytes()[loc + 8..loc + 10].copy_from_slice(&chopped.to_le_bytes());
                if prefix > 0 {
                    bits.bytes()[written..written + 3].copy_from_slice(&[0, 0, 1]);
                }
                bits.bytes()[written + prefix..written + prefix + take]
                    .copy_from_slice(&slices[slice][offset..offset + take]);
                written += prefix + take;
                offset += take;
                n += 1;
                if offset == slices[slice].len() {
                    slice += 1;
                    offset = 0;
                }
                if written == capacity {
                    break;
                }
            }
            ensure!(written > 0, "empty DXVA submission");
            if written % 128 != 0 {
                let padding = 128 - written % 128;
                bits.bytes()[written..written + padding].fill(0);
                written += padding;
                control.bytes()[(n - 1) * 10 + 4..(n - 1) * 10 + 8]
                    .copy_from_slice(&((last_size + padding) as u32).to_le_bytes());
            }
            control.release()?;
            bits.release()?;
            desc.push(D3D11_VIDEO_DECODER_BUFFER_DESC {
                BufferType: D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
                DataSize: written as u32,
                ..Default::default()
            });
            desc.push(D3D11_VIDEO_DECODER_BUFFER_DESC {
                BufferType: D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
                DataSize: (n * 10) as u32,
                ..Default::default()
            });
            unsafe {
                self.video.SubmitDecoderBuffers(&self.decoder, &desc)?;
            }
            first = false;
        }
        Ok(())
    }
}
fn create_texture(device: &ID3D11Device, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D> {
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(desc, None, Some(&mut texture))?;
    }
    texture.context("missing texture")
}
