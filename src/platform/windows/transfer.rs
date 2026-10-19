//! Device delivery: same-adapter shared texture or cross-adapter D3D12 heap.
//! Busy slots drop an input; only an unavailable/failed GPU strategy reads back.
mod local;
use super::capture::Frame;
use anyhow::{Context, Result, ensure};
use std::{
    mem::ManuallyDrop,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Graphics::{
            Direct3D::D3D_FEATURE_LEVEL_11_0,
            Direct3D11::*,
            Direct3D12::*,
            Dxgi::{Common::*, IDXGIDevice},
        },
    },
    core::{Interface, PCWSTR},
};
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
pub(crate) struct Delivery {
    pub frame: Frame,
    retire: Option<(Arc<Slot>, u64)>,
    local: Option<local::Lease>,
}
impl Drop for Delivery {
    fn drop(&mut self) {
        drop(self.local.take());
        if let Some((slot, value)) = self.retire.take() {
            unsafe {
                slot.retired.store(value, Ordering::Release);
                if slot
                    .destination
                    .context
                    .Signal(&slot.consumed, value)
                    .is_err()
                {
                    slot.failed.store(true, Ordering::Release);
                }
                slot.destination.context.Flush();
                slot.leased.store(false, Ordering::Release);
            }
        }
    }
}
pub(crate) struct Transfer {
    source: ID3D11Device,
    destination: ID3D11Device,
    engine: Engine,
    size: (u32, u32, DXGI_FORMAT),
}
enum Engine {
    Local(local::Transfer),
    Shared(Gpu),
    Staging(Staging),
}
impl Transfer {
    pub fn new(source: &ID3D11Device, destination: &ID3D11Device, frame: &Frame) -> Result<Self> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            frame.texture.GetDesc(&mut desc);
        }
        let same_adapter = unsafe {
            source
                .cast::<IDXGIDevice>()?
                .GetAdapter()?
                .GetDesc()?
                .AdapterLuid
                == destination
                    .cast::<IDXGIDevice>()?
                    .GetAdapter()?
                    .GetDesc()?
                    .AdapterLuid
        };
        let engine = if same_adapter {
            match local::Transfer::new(source, destination, desc) {
                Ok(bridge) => {
                    tracing::info!(
                        "host texture delivery: same-adapter shared texture with GPU fences"
                    );
                    Engine::Local(bridge)
                }
                Err(error) => {
                    tracing::warn!(%error, "same-adapter GPU bridge unavailable; selecting staging");
                    Engine::Staging(Staging::new(source, destination, desc)?)
                }
            }
        } else {
            match Gpu::new(source, destination, desc, true)
                .or_else(|_| Gpu::new(source, destination, desc, false))
            {
                Ok(bridge) => Engine::Shared(bridge),
                Err(error) => {
                    tracing::debug!(%error,"cross-adapter GPU bridge unavailable; selecting staging");
                    Engine::Staging(Staging::new(source, destination, desc)?)
                }
            }
        };
        Ok(Self {
            source: source.clone(),
            destination: destination.clone(),
            engine,
            size: (desc.Width, desc.Height, desc.Format),
        })
    }
    pub fn matches(
        &self,
        source: &ID3D11Device,
        destination: &ID3D11Device,
        frame: &Frame,
    ) -> bool {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            frame.texture.GetDesc(&mut desc);
        }
        self.source == *source
            && self.destination == *destination
            && self.size == (desc.Width, desc.Height, desc.Format)
    }
    pub fn copy(&mut self, frame: &Frame, active: impl Fn() -> bool) -> Result<Option<Delivery>> {
        ensure!(active(), "GPU交付已取消");
        match &mut self.engine {
            Engine::Local(gpu) => match gpu.copy(frame) {
                Ok(frame) => return Ok(frame),
                Err(error) => {
                    tracing::warn!(%error, "same-adapter GPU strategy failed; selecting staging")
                }
            },
            Engine::Shared(gpu) => match gpu.copy(frame) {
                Ok(frame) => return Ok(frame),
                Err(error) => {
                    tracing::debug!(%error,"cross-adapter GPU strategy failed; selecting staging")
                }
            },
            Engine::Staging(staging) => return staging.copy(frame, active).map(Some),
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            frame.texture.GetDesc(&mut desc);
        }
        self.engine = Engine::Staging(Staging::new(&self.source, &self.destination, desc)?);
        self.copy(frame, active)
    }
}
struct Staging {
    source: ID3D11DeviceContext,
    destination: ID3D11DeviceContext,
    read: ID3D11Texture2D,
    write: ID3D11Texture2D,
}
impl Staging {
    fn new(
        source: &ID3D11Device,
        destination: &ID3D11Device,
        desc: D3D11_TEXTURE2D_DESC,
    ) -> Result<Self> {
        unsafe {
            let (mut read, mut write) = (None, None);
            source.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    MiscFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..desc
                },
                None,
                Some(&mut read),
            )?;
            destination.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    MiscFlags: 0,
                    CPUAccessFlags: 0,
                    ..desc
                },
                None,
                Some(&mut write),
            )?;
            Ok(Self {
                source: source.GetImmediateContext()?,
                destination: destination.GetImmediateContext()?,
                read: read.context("跨卡读回纹理")?,
                write: write.context("跨卡上传纹理")?,
            })
        }
    }
    fn copy(&self, frame: &Frame, active: impl Fn() -> bool) -> Result<Delivery> {
        unsafe {
            self.source.CopyResource(&self.read, &frame.texture);
            let mapped = super::gpu_readback::map(&self.source, &self.read, active)?;
            if mapped.pData.is_null() {
                self.source.Unmap(&self.read, 0);
                anyhow::bail!("跨卡读回未返回像素");
            }
            self.destination.UpdateSubresource(
                &self.write,
                0,
                None,
                mapped.pData,
                mapped.RowPitch,
                0,
            );
            self.source.Unmap(&self.read, 0);
            let mut frame = frame.clone();
            frame.texture = self.write.clone();
            Ok(Delivery {
                frame,
                retire: None,
                local: None,
            })
        }
    }
}
struct Endpoint {
    device: ID3D12Device,
    context: ID3D11DeviceContext4,
    queue: ID3D12CommandQueue,
}
impl Endpoint {
    fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
            let mut twelve = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut twelve)?;
            let twelve: ID3D12Device = twelve.context("跨卡D3D12设备")?;
            let queue = twelve.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COPY,
                ..Default::default()
            })?;
            Ok(Self {
                device: twelve,
                context: device.GetImmediateContext()?.cast()?,
                queue,
            })
        }
    }
}
struct Slot {
    source: Arc<Endpoint>,
    destination: Arc<Endpoint>,
    source_texture: ID3D11Texture2D,
    destination_texture: ID3D11Texture2D,
    source_fence: ID3D12Fence,
    source_fence11: ID3D11Fence,
    cross_fence: ID3D12Fence,
    remote_cross_fence: ID3D12Fence,
    destination_fence: ID3D12Fence,
    destination_fence11: ID3D11Fence,
    consumed: ID3D11Fence,
    source_commands: ID3D12CommandList,
    destination_commands: ID3D12CommandList,
    _allocators: [ID3D12CommandAllocator; 2],
    _resources: Vec<ID3D12Resource>,
    _heaps: [ID3D12Heap; 2],
    value: AtomicU64,
    source_issued: AtomicU64,
    cross_issued: AtomicU64,
    destination_issued: AtomicU64,
    retired: AtomicU64,
    failed: AtomicBool,
    leased: AtomicBool,
}
impl Slot {
    fn idle(&self) -> bool {
        unsafe {
            let value = self.value.load(Ordering::Acquire);
            value == 0
                || (!self.failed.load(Ordering::Acquire)
                    && self.retired.load(Ordering::Acquire) == value
                    && self.consumed.GetCompletedValue() >= value)
        }
    }
    fn quiescent(&self) -> bool {
        unsafe {
            !self.leased.load(Ordering::Acquire)
                && self.source_fence.GetCompletedValue()
                    >= self.source_issued.load(Ordering::Acquire)
                && self.cross_fence.GetCompletedValue() >= self.cross_issued.load(Ordering::Acquire)
                && self.destination_fence.GetCompletedValue()
                    >= self.destination_issued.load(Ordering::Acquire)
                && self.consumed.GetCompletedValue() >= self.retired.load(Ordering::Acquire)
        }
    }
}
struct Gpu {
    slots: Vec<Arc<Slot>>,
    next: usize,
}
impl Gpu {
    fn new(
        source: &ID3D11Device,
        destination: &ID3D11Device,
        desc: D3D11_TEXTURE2D_DESC,
        row_major: bool,
    ) -> Result<Self> {
        let source12 = Arc::new(Endpoint::new(source)?);
        let destination12 = Arc::new(Endpoint::new(destination)?);
        if row_major {
            for endpoint in [&source12, &destination12] {
                unsafe {
                    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
                    endpoint.device.CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS,
                        std::ptr::from_mut(&mut options).cast(),
                        std::mem::size_of_val(&options) as u32,
                    )?;
                    ensure!(
                        options.CrossAdapterRowMajorTextureSupported.as_bool(),
                        "适配器不支持跨卡row-major纹理"
                    );
                }
            }
        }
        let mut slots = Vec::new();
        for _ in 0..2 {
            slots.push(Arc::new(Self::slot(
                source,
                destination,
                source12.clone(),
                destination12.clone(),
                desc,
                row_major,
            )?));
        }
        Ok(Self { slots, next: 0 })
    }
    fn slot(
        source11: &ID3D11Device,
        destination11: &ID3D11Device,
        source: Arc<Endpoint>,
        destination: Arc<Endpoint>,
        input: D3D11_TEXTURE2D_DESC,
        row_major: bool,
    ) -> Result<Slot> {
        unsafe {
            let normal = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: input.Width.into(),
                Height: input.Height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: input.Format,
                SampleDesc: input.SampleDesc,
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS
                    | D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                ..Default::default()
            };
            let (source_resource, source_texture) = interop(&source.device, source11, &normal)?;
            let (destination_resource, destination_texture) =
                interop(&destination.device, destination11, &normal)?;
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut bytes = 0;
            source.device.GetCopyableFootprints(
                &normal,
                0,
                1,
                0,
                Some(&mut footprint),
                None,
                None,
                Some(&mut bytes),
            );
            let cross = if row_major {
                D3D12_RESOURCE_DESC {
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER
                        | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
                    ..normal
                }
            } else {
                D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: bytes,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_UNKNOWN,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
                    ..Default::default()
                }
            };
            let local_info = source.device.GetResourceAllocationInfo(0, &[cross]);
            let remote_info = destination.device.GetResourceAllocationInfo(0, &[cross]);
            let alignment = local_info.Alignment.max(remote_info.Alignment);
            let size = local_info.SizeInBytes.max(remote_info.SizeInBytes);
            ensure!(
                alignment > 0 && size > 0 && size < u64::MAX - alignment,
                "跨卡共享内存尺寸无效"
            );
            let mut heap = None;
            source.device.CreateHeap(
                &D3D12_HEAP_DESC {
                    SizeInBytes: size.div_ceil(alignment) * alignment,
                    Alignment: alignment,
                    Properties: D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_CUSTOM,
                        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE,
                        MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                        CreationNodeMask: 1,
                        VisibleNodeMask: 1,
                    },
                    Flags: D3D12_HEAP_FLAG_SHARED | D3D12_HEAP_FLAG_SHARED_CROSS_ADAPTER,
                },
                &mut heap,
            )?;
            let heap: ID3D12Heap = heap.context("创建跨卡heap")?;
            let shared = Handle(source.device.CreateSharedHandle(
                &heap,
                None,
                0x10000000,
                PCWSTR::null(),
            )?);
            let mut remote_heap = None;
            destination
                .device
                .OpenSharedHandle(shared.0, &mut remote_heap)?;
            let remote_heap: ID3D12Heap = remote_heap.context("导入跨卡heap")?;
            let (mut local_resource, mut remote_resource) = (None, None);
            source.device.CreatePlacedResource(
                &heap,
                0,
                &cross,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut local_resource,
            )?;
            destination.device.CreatePlacedResource(
                &remote_heap,
                0,
                &cross,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut remote_resource,
            )?;
            let local_resource: ID3D12Resource = local_resource.context("创建跨卡源resource")?;
            let remote_resource: ID3D12Resource =
                remote_resource.context("创建跨卡目标resource")?;
            let (source_commands, source_allocator) = commands(
                &source.device,
                &source_resource,
                None,
                &local_resource,
                (!row_major).then_some(footprint),
            )?;
            let (destination_commands, destination_allocator) = commands(
                &destination.device,
                &remote_resource,
                (!row_major).then_some(footprint),
                &destination_resource,
                None,
            )?;
            let (source_fence, source_fence11) = fence(&source.device, source11)?;
            let (destination_fence, destination_fence11) =
                fence(&destination.device, destination11)?;
            let cross_fence: ID3D12Fence = source.device.CreateFence(
                0,
                D3D12_FENCE_FLAG_SHARED | D3D12_FENCE_FLAG_SHARED_CROSS_ADAPTER,
            )?;
            let shared = Handle(source.device.CreateSharedHandle(
                &cross_fence,
                None,
                0x10000000,
                PCWSTR::null(),
            )?);
            let mut remote_cross_fence = None;
            destination
                .device
                .OpenSharedHandle(shared.0, &mut remote_cross_fence)?;
            let mut consumed = None;
            destination11.cast::<ID3D11Device5>()?.CreateFence(
                0,
                D3D11_FENCE_FLAG_NONE,
                &mut consumed,
            )?;
            Ok(Slot {
                source,
                destination,
                source_texture,
                destination_texture,
                source_fence,
                source_fence11,
                cross_fence,
                remote_cross_fence: remote_cross_fence.context("导入跨卡fence")?,
                destination_fence,
                destination_fence11,
                consumed: consumed.context("创建编码读取fence")?,
                source_commands,
                destination_commands,
                _allocators: [source_allocator, destination_allocator],
                _resources: vec![
                    source_resource,
                    destination_resource,
                    local_resource,
                    remote_resource,
                ],
                _heaps: [heap, remote_heap],
                value: AtomicU64::new(0),
                source_issued: AtomicU64::new(0),
                cross_issued: AtomicU64::new(0),
                destination_issued: AtomicU64::new(0),
                retired: AtomicU64::new(0),
                failed: AtomicBool::new(false),
                leased: AtomicBool::new(false),
            })
        }
    }
    fn copy(&mut self, frame: &Frame) -> Result<Option<Delivery>> {
        unsafe {
            ensure!(
                !self.slots.iter().any(|s| s.failed.load(Ordering::Acquire)),
                "跨卡读取完成fence失效"
            );
            let Some(index) = (0..self.slots.len())
                .map(|i| (self.next + i) % self.slots.len())
                .find(|&i| self.slots[i].idle())
            else {
                return Ok(None);
            };
            self.next = (index + 1) % self.slots.len();
            let slot = &self.slots[index];
            let value = slot.value.fetch_add(1, Ordering::AcqRel) + 1;
            slot.source
                .context
                .CopyResource(&slot.source_texture, &frame.texture);
            slot.source_issued.store(value, Ordering::Release);
            slot.source.context.Signal(&slot.source_fence11, value)?;
            slot.source.context.Flush();
            slot.source.queue.Wait(&slot.source_fence, value)?;
            slot.cross_issued.store(value, Ordering::Release);
            slot.source
                .queue
                .ExecuteCommandLists(&[Some(slot.source_commands.clone())]);
            slot.source.queue.Signal(&slot.cross_fence, value)?;
            slot.destination
                .queue
                .Wait(&slot.remote_cross_fence, value)?;
            slot.destination_issued.store(value, Ordering::Release);
            slot.destination
                .queue
                .ExecuteCommandLists(&[Some(slot.destination_commands.clone())]);
            slot.destination
                .queue
                .Signal(&slot.destination_fence, value)?;
            slot.destination
                .context
                .Wait(&slot.destination_fence11, value)?;
            slot.leased.store(true, Ordering::Release);
            let mut output = frame.clone();
            output.texture = slot.destination_texture.clone();
            Ok(Some(Delivery {
                frame: output,
                retire: Some((slot.clone(), value)),
                local: None,
            }))
        }
    }
}
impl Drop for Gpu {
    fn drop(&mut self) {
        let slots = Arc::new(std::mem::take(&mut self.slots));
        if slots.iter().all(|s| s.quiescent()) {
            return;
        }
        // Keep every command allocator/resource alive until its submitted work is
        // complete. A slow driver must not block the UI closing a viewing session.
        let keep = slots.clone();
        if std::thread::Builder::new()
            .name("host-gpu-retire".into())
            .spawn(move || retire(&keep))
            .is_err()
        {
            retire(&slots);
        }
    }
}
fn retire(slots: &[Arc<Slot>]) {
    while slots.iter().any(|s| !s.quiescent()) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
unsafe fn interop(
    twelve: &ID3D12Device,
    eleven: &ID3D11Device,
    desc: &D3D12_RESOURCE_DESC,
) -> Result<(ID3D12Resource, ID3D11Texture2D)> {
    unsafe {
        let mut resource = None;
        twelve.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                CreationNodeMask: 1,
                VisibleNodeMask: 1,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_SHARED,
            desc,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut resource,
        )?;
        let resource: ID3D12Resource = resource.context("创建D3D11/12互操作纹理")?;
        let handle =
            Handle(twelve.CreateSharedHandle(&resource, None, 0x10000000, PCWSTR::null())?);
        let texture = eleven
            .cast::<ID3D11Device1>()?
            .OpenSharedResource1(handle.0)?;
        Ok((resource, texture))
    }
}
unsafe fn fence(
    twelve: &ID3D12Device,
    eleven: &ID3D11Device,
) -> Result<(ID3D12Fence, ID3D11Fence)> {
    unsafe {
        let fence: ID3D12Fence = twelve.CreateFence(0, D3D12_FENCE_FLAG_SHARED)?;
        let handle = Handle(twelve.CreateSharedHandle(&fence, None, 0x10000000, PCWSTR::null())?);
        let mut other = None;
        eleven
            .cast::<ID3D11Device5>()?
            .OpenSharedFence(handle.0, &mut other)?;
        Ok((fence, other.context("导入D3D11/12 fence")?))
    }
}
unsafe fn commands(
    device: &ID3D12Device,
    source: &ID3D12Resource,
    source_footprint: Option<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    destination: &ID3D12Resource,
    destination_footprint: Option<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
) -> Result<(ID3D12CommandList, ID3D12CommandAllocator)> {
    unsafe {
        let allocator: ID3D12CommandAllocator =
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
        let list: ID3D12GraphicsCommandList =
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
        let location =
            |resource: &ID3D12Resource, footprint: Option<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>| {
                D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(resource.clone())),
                    Type: if footprint.is_some() {
                        D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT
                    } else {
                        D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX
                    },
                    Anonymous: if let Some(footprint) = footprint {
                        D3D12_TEXTURE_COPY_LOCATION_0 {
                            PlacedFootprint: footprint,
                        }
                    } else {
                        D3D12_TEXTURE_COPY_LOCATION_0 {
                            SubresourceIndex: 0,
                        }
                    },
                }
            };
        let mut src = location(source, source_footprint);
        let mut dst = location(destination, destination_footprint);
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        ManuallyDrop::drop(&mut src.pResource);
        ManuallyDrop::drop(&mut dst.pResource);
        list.Close()?;
        Ok((list.cast()?, allocator))
    }
}
