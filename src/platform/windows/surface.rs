use crate::i18n::t;
use std::sync::{Arc, Mutex};

use crate::platform::decoder::WindowsGpuVideoFrame;
use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
    IDXGIAdapter1, IDXGIDevice, IDXGIDevice1, IDXGIFactory1, IDXGIFactory6, IDXGIKeyedMutex,
    IDXGIResource,
};
use windows::core::Interface;

#[derive(Clone)]
pub(crate) struct D3D11SurfaceWriter {
    shared: Arc<D3D11Shared>,
}

pub(crate) struct D3D11Surface {
    _frame: Option<WindowsGpuVideoFrame>,
    visible_origin: (u32, u32),
    texture: ID3D11Texture2D,
    subresource: u32,
    desc: D3D11_TEXTURE2D_DESC,
    // Shader interpretation can differ from storage for CPU-produced packed YUV.
    pixel_format: DXGI_FORMAT,
    shared_handle: Option<isize>,
    shared: Arc<D3D11Shared>,
}

struct D3D11Shared {
    adapter: u64,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    reported_format: Mutex<Option<windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT>>,
}

pub(crate) fn acquire_texture_sync(sync: &IDXGIKeyedMutex, timeout_ms: u32) -> Result<()> {
    // AcquireSync returns positive WAIT_TIMEOUT/WAIT_ABANDONED values too;
    // the generated Result wrapper treats those as successful HRESULTs.
    let status = unsafe { (Interface::vtable(sync).AcquireSync)(sync.as_raw(), 0, timeout_ms) };
    if status != windows::core::HRESULT(0) {
        bail!("acquire shared video texture failed: {status:?}");
    }
    Ok(())
}

pub(crate) fn acquire_owned_texture_sync(
    texture: &ID3D11Texture2D,
    timeout_ms: u32,
) -> Result<Option<IDXGIKeyedMutex>> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut desc);
    }
    if desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32 == 0 {
        return Ok(None);
    }
    // Shared keyed resources require ownership even on their creating device.
    // The presenter may adopt the creating device for non-shared resources.
    let sync: IDXGIKeyedMutex = texture
        .cast()
        .context("query owning-device texture mutex")?;
    acquire_texture_sync(&sync, timeout_ms)?;
    Ok(Some(sync))
}

// The device is created with D3D11 multithread protection enabled. All immediate-context
// calls are serialized by the driver; format-reporting metadata uses a Mutex.
// The raw COM wrappers are reference-counted and remain alive through this Arc.
unsafe impl Send for D3D11Shared {}
unsafe impl Sync for D3D11Shared {}

impl std::fmt::Debug for D3D11Surface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("D3D11Surface")
            .field("width", &self.desc.Width)
            .field("height", &self.desc.Height)
            .field("pixel_format", &self.pixel_format)
            .field("storage_format", &self.desc.Format)
            .finish_non_exhaustive()
    }
}

impl D3D11SurfaceWriter {
    /// Diagnostic inventory retains per-device creation errors and never falls
    /// back to a different adapter. IDD devices are display paths, not extra GPUs.
    pub(crate) fn diagnostic_adapters() -> Result<Vec<(String, Result<Self>)>> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut devices = Vec::new();
        for index in 0..64 {
            let adapter = match unsafe { factory.EnumAdapters1(index) } {
                Ok(adapter) => adapter,
                Err(error)
                    if error.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_NOT_FOUND =>
                {
                    break;
                }
                Err(error) => {
                    devices.push((
                        (t().system.gpu_adapter_enumeration)(index + 1),
                        Err(error.into()),
                    ));
                    break;
                }
            };
            let desc = match unsafe { adapter.GetDesc1() } {
                Ok(desc) => desc,
                Err(error) => {
                    devices.push(((t().system.gpu_adapter)(index + 1), Err(error.into())));
                    continue;
                }
            };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
                || super::adapter_type::indirect(desc.AdapterLuid) == Some(true)
            {
                continue;
            }
            let name = String::from_utf16_lossy(
                &desc.Description[..desc
                    .Description
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(desc.Description.len())],
            );
            let label = if devices.iter().any(|(label, _)| label == &name) {
                (t().system.gpu_adapter_named)(&name, index + 1)
            } else {
                name
            };
            devices.push((label, Self::from_adapter(&adapter)));
        }
        Ok(devices)
    }
    pub(crate) fn new() -> Result<Self> {
        Self::available()?
            .into_iter()
            .next()
            .context("no usable D3D11 adapter")
    }

    pub(crate) fn available() -> Result<Vec<Self>> {
        let mut writers = Vec::new();
        for adapter in Self::adapters()? {
            match Self::from_adapter(&adapter) {
                Ok(writer) => writers.push(writer),
                Err(error) => tracing::debug!(%error, "D3D11 adapter unavailable"),
            }
        }
        if writers.is_empty() {
            bail!("no usable hardware D3D11 adapter");
        }
        Ok(writers)
    }

    pub(crate) fn adapter_ids() -> Result<Vec<u64>> {
        Self::adapters()?.iter().map(adapter_id).collect()
    }

    pub(crate) fn for_adapter(id: u64) -> Result<Self> {
        for adapter in Self::adapters()? {
            if adapter_id(&adapter)? == id {
                return Self::from_adapter(&adapter);
            }
        }
        bail!("requested D3D11 adapter {id:016x} is unavailable")
    }

    pub(crate) fn adapter_id(&self) -> u64 {
        self.shared.adapter
    }

    fn adapters() -> Result<Vec<IDXGIAdapter1>> {
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1() }.context("enumerate D3D11 adapters")?;
        let preferred = factory.cast::<IDXGIFactory6>().ok();
        let mut adapters = Vec::new();
        for index in 0.. {
            let adapter: windows::core::Result<IDXGIAdapter1> = unsafe {
                match &preferred {
                    Some(factory) => factory
                        .EnumAdapterByGpuPreference(index, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE),
                    None => factory.EnumAdapters1(index),
                }
            };
            let Ok(adapter) = adapter else {
                break;
            };
            let desc = unsafe { adapter.GetDesc1()? };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
                || super::adapter_type::indirect(desc.AdapterLuid) == Some(true)
            {
                continue;
            }
            adapters.push(adapter);
        }
        Ok(adapters)
    }

    fn from_adapter(adapter: &IDXGIAdapter1) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        let mut result = Ok(());
        for flags in [
            flags,
            windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG(0),
        ] {
            device = None;
            context = None;
            result = unsafe {
                D3D11CreateDevice(
                    adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&raw mut device),
                    None,
                    Some(&raw mut context),
                )
            };
            if result.is_ok() {
                break;
            }
        }
        result.context("create D3D11 video device")?;
        let device = device.context("D3D11 did not return a device")?;
        let context = context.context("D3D11 did not return an immediate context")?;
        configure_device(&device, true);
        Ok(Self {
            shared: Arc::new(D3D11Shared {
                adapter: adapter_id(adapter)?,
                device,
                context,
                reported_format: Mutex::new(None),
            }),
        })
    }

    pub(crate) fn device_handle(&self) -> ID3D11Device {
        self.shared.device.clone()
    }

    pub(crate) fn create_renderer_device(&self) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        create_renderer_device(&self.shared.device)
    }

    pub(crate) fn supports_software_av1(&self, depth: u8, chroma: u8) -> bool {
        use crate::platform::decoder::{WindowsCpuFormat as F, WindowsCpuVideoFrame};
        let (format, len) = match (chroma, depth) {
            (1, 8) => (F::Nv12, 6),
            (1, 10) => (F::P010, 12),
            (3, 8) => (F::Ayuv, 16),
            (3, 10) => (F::Y410, 16),
            _ => return false,
        };
        self.upload_cpu(&WindowsCpuVideoFrame {
            pts: 0,
            width: 2,
            height: 2,
            coded_width: 2,
            coded_height: 2,
            format,
            data: bytes::Bytes::from(vec![0; len]),
        })
        .is_ok()
    }

    pub(crate) fn upload_cpu(
        &self,
        frame: &crate::platform::decoder::WindowsCpuVideoFrame,
    ) -> Result<D3D11Surface> {
        use crate::platform::decoder::WindowsCpuFormat;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_DEFAULT,
        };
        let (format, bytes, planar) = match frame.format {
            WindowsCpuFormat::Nv12 => (DXGI_FORMAT_NV12, 1, true),
            WindowsCpuFormat::P010 => (DXGI_FORMAT_P010, 2, true),
            WindowsCpuFormat::Ayuv => (DXGI_FORMAT_AYUV, 4, false),
            WindowsCpuFormat::Y410 => (DXGI_FORMAT_Y410, 4, false),
            WindowsCpuFormat::I444 => bail!("planar I444 is not a packed upload format"),
        };
        let (w, h) = (frame.coded_width, frame.coded_height);
        anyhow::ensure!(
            w > 0 && h > 0 && w >= frame.width && h >= frame.height,
            "invalid software video dimensions"
        );
        anyhow::ensure!(
            !planar || w % 2 == 0 && h % 2 == 0,
            "invalid planar upload alignment"
        );
        let pitch = w
            .checked_mul(bytes)
            .context("software video row overflow")?;
        let rows = if planar {
            h.checked_add(h / 2)
                .context("software video height overflow")?
        } else {
            h
        };
        let length = (pitch as usize)
            .checked_mul(rows as usize)
            .context("software video allocation overflow")?;
        anyhow::ensure!(
            frame.data.len() == length,
            "software video data length mismatch"
        );
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            // Packed CPU pixels only need a shader-readable bit layout; native
            // AYUV/Y410 video resources are optional even on modern adapters.
            Format: match format {
                DXGI_FORMAT_AYUV => DXGI_FORMAT_R8G8B8A8_UNORM,
                DXGI_FORMAT_Y410 => DXGI_FORMAT_R10G10B10A2_UNORM,
                _ => format,
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let input = D3D11_SUBRESOURCE_DATA {
            pSysMem: frame.data.as_ptr().cast(),
            SysMemPitch: pitch,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        unsafe {
            self.shared
                .device
                .CreateTexture2D(&desc, Some(&input), Some(&mut texture))
        }
        .context("upload software decoded video")?;
        Ok(D3D11Surface {
            texture: texture.context("missing uploaded video texture")?,
            subresource: 0,
            desc,
            pixel_format: format,
            shared_handle: None,
            _frame: None,
            visible_origin: (0, 0),
            shared: Arc::clone(&self.shared),
        })
    }

    pub(crate) fn wrap_decoded_surface(&self, frame: WindowsGpuVideoFrame) -> Result<D3D11Surface> {
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { frame.texture().GetDesc(&raw mut source_desc) };
        if !matches!(
            source_desc.Format,
            DXGI_FORMAT_NV12 | DXGI_FORMAT_P010 | DXGI_FORMAT_AYUV | DXGI_FORMAT_Y410
        ) {
            bail!(
                "Windows decoder returned unsupported D3D11 format {:?}",
                source_desc.Format
            );
        }
        let mut reported = self
            .shared
            .reported_format
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reported.replace(source_desc.Format) != Some(source_desc.Format) {
            tracing::info!(
                format = ?source_desc.Format,
                coded_width = source_desc.Width,
                coded_height = source_desc.Height,
                array_size = source_desc.ArraySize,
                bind_flags = source_desc.BindFlags,
                misc_flags = source_desc.MiscFlags,
                visible_x = frame.visible_x(),
                visible_y = frame.visible_y(),
                visible_width = frame.width(),
                visible_height = frame.height(),
                "Windows decoder output surface format changed"
            );
        }
        drop(reported);
        let source_is_shared =
            source_desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32 != 0;
        let shared_handle = if source_is_shared {
            let resource: IDXGIResource = frame
                .texture()
                .cast()
                .context("query decoder shared DXGI resource")?;
            Some(
                unsafe { resource.GetSharedHandle() }
                    .context("get decoder shared texture handle")?
                    .0 as isize,
            )
        } else {
            None
        };
        // Keep the decoder sample/subresource intact. The presenter adopts this
        // device for non-shared input.
        Ok(D3D11Surface {
            texture: frame.texture().clone(),
            subresource: frame.subresource(),
            desc: source_desc,
            pixel_format: source_desc.Format,
            shared_handle,
            visible_origin: (frame.visible_x(), frame.visible_y()),
            _frame: Some(frame),
            shared: Arc::clone(&self.shared),
        })
    }
}

fn adapter_id(adapter: &IDXGIAdapter1) -> Result<u64> {
    let id = unsafe { adapter.GetDesc1()? }.AdapterLuid;
    Ok((u64::from(id.HighPart as u32) << 32) | u64::from(id.LowPart))
}

fn create_renderer_device(
    decoder_device: &ID3D11Device,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let dxgi_device: IDXGIDevice = decoder_device.cast().context("query decoder DXGI device")?;
    let adapter = unsafe { dxgi_device.GetAdapter() }.context("get decoder DXGI adapter")?;
    let mut device = None;
    let mut context = None;
    let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            None,
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            Some(&raw mut context),
        )
    }
    .context("create isolated D3D11 renderer device on decoder adapter")?;
    let device = device.context("D3D11 did not return an isolated renderer device")?;
    let context = context.context("D3D11 did not return an isolated renderer context")?;
    configure_device(&device, false);
    Ok((device, context))
}

fn configure_device(device: &ID3D11Device, limit_device_latency: bool) {
    if let Ok(multithread) = unsafe { device.GetImmediateContext() }
        .and_then(|context| context.cast::<ID3D11Multithread>())
    {
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
    }
    if let Ok(dxgi_device) = device.cast::<IDXGIDevice>()
        && let Err(error) = unsafe { dxgi_device.SetGPUThreadPriority(7) }
    {
        tracing::warn!(%error, "failed to apply the DXGI GPU priority");
    }
    if limit_device_latency
        && let Ok(dxgi_device) = device.cast::<IDXGIDevice1>()
        && let Err(error) = unsafe { dxgi_device.SetMaximumFrameLatency(1) }
    {
        tracing::warn!(%error, "failed to apply the decoder frame-latency limit");
    }
}

impl D3D11Surface {
    pub(crate) fn adapter_id(&self) -> u64 {
        self.shared.adapter
    }

    /// Hash of the visible luma plane, read back through a staging copy
    /// (`dev video` compares two viewers of one stream). Call on the thread
    /// that owns the device's immediate context.
    pub(crate) fn luma_hash(&self, width: u32, height: u32) -> Result<u64> {
        use std::hash::Hasher;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_STAGING,
        };
        let mut desc = self.desc;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let bytes_per_sample = match desc.Format {
            DXGI_FORMAT_NV12 => 1,
            DXGI_FORMAT_P010 | DXGI_FORMAT_P016 => 2,
            _ => 4,
        };
        let (x, y) = self.visible_origin;
        anyhow::ensure!(x + width <= desc.Width && y + height <= desc.Height, "visible area outside the surface");
        unsafe {
            let mut staging = None;
            self.shared.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
            let staging = staging.context("staging texture")?;
            let context = self.context();
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &self.texture, self.subresource, None);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for row in y..y + height {
                let start = mapped.pData.cast::<u8>().add(row as usize * mapped.RowPitch as usize);
                let line = std::slice::from_raw_parts(
                    start.add(x as usize * bytes_per_sample),
                    width as usize * bytes_per_sample,
                );
                hasher.write(line);
            }
            context.Unmap(&staging, 0);
            Ok(hasher.finish())
        }
    }

    pub(crate) fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    pub(crate) const fn subresource(&self) -> u32 {
        self.subresource
    }

    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.shared.device
    }

    pub(crate) fn belongs_to_device(&self, device: &ID3D11Device) -> bool {
        Interface::as_raw(&self.shared.device) == Interface::as_raw(device)
    }

    pub(crate) const fn shared_handle(&self) -> Option<isize> {
        self.shared_handle
    }

    pub(crate) fn create_renderer_device(&self) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        create_renderer_device(&self.shared.device)
    }

    pub(crate) fn context(&self) -> &ID3D11DeviceContext {
        &self.shared.context
    }

    /// Allocation dimensions reported by the decoder texture. Hardware
    /// decoders may align these beyond the visible picture dimensions (for
    /// example a 1920x1200 picture in a 1920x1216 NV12 allocation).
    pub(crate) fn coded_size(&self) -> (u32, u32) {
        (self.desc.Width, self.desc.Height)
    }

    pub(crate) const fn visible_origin(&self) -> (u32, u32) {
        self.visible_origin
    }

    pub(crate) fn format(&self) -> windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
        self.pixel_format
    }
}
