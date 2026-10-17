//! Same-adapter handoff without CPU readback or cross-adapter copy queues.
//! D3D12 only allocates the shared surface; all submitted work belongs to the
//! two D3D11 contexts, whose runtime retains submitted resource references.
use super::*;

pub(super) struct Transfer {
    slots: Vec<Arc<Slot>>,
    next: usize,
}

struct Slot {
    source: ID3D11DeviceContext4,
    destination: ID3D11DeviceContext4,
    write: ID3D11Texture2D,
    read: ID3D11Texture2D,
    ready: ID3D11Fence,
    remote_ready: ID3D11Fence,
    consumed: ID3D11Fence,
    value: AtomicU64,
    leased: AtomicBool,
    failed: AtomicBool,
    _allocation: ID3D12Resource,
}

pub(super) struct Lease {
    slot: Arc<Slot>,
    value: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        unsafe {
            // This runs after the encoder has submitted every read of the
            // source. A slot cannot be overwritten until those reads finish.
            if self
                .slot
                .destination
                .Signal(&self.slot.consumed, self.value)
                .is_err()
            {
                self.slot.failed.store(true, Ordering::Release);
            }
            self.slot.destination.Flush();
        }
        self.slot.leased.store(false, Ordering::Release);
    }
}

impl Transfer {
    pub(super) fn new(
        source: &ID3D11Device,
        destination: &ID3D11Device,
        input: D3D11_TEXTURE2D_DESC,
    ) -> Result<Self> {
        ensure!(
            input.MipLevels == 1 && input.ArraySize == 1 && input.SampleDesc.Count == 1,
            "同卡交付需要单层非多重采样纹理"
        );
        unsafe {
            let adapter = source.cast::<IDXGIDevice>()?.GetAdapter()?;
            let mut allocator = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut allocator)?;
            let allocator: ID3D12Device = allocator.context("同卡共享纹理分配设备")?;
            let source_context: ID3D11DeviceContext4 = source.GetImmediateContext()?.cast()?;
            let destination_context: ID3D11DeviceContext4 =
                destination.GetImmediateContext()?.cast()?;
            let mut slots = Vec::with_capacity(2);
            for _ in 0..2 {
                let (allocation, write) = interop(
                    &allocator,
                    source,
                    &D3D12_RESOURCE_DESC {
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
                    },
                )?;
                let shared = Handle(allocator.CreateSharedHandle(
                    &allocation,
                    None,
                    0x10000000,
                    PCWSTR::null(),
                )?);
                let read = destination
                    .cast::<ID3D11Device1>()?
                    .OpenSharedResource1(shared.0)?;
                let (ready12, ready) = fence(&allocator, source)?;
                let shared = Handle(allocator.CreateSharedHandle(
                    &ready12,
                    None,
                    0x10000000,
                    PCWSTR::null(),
                )?);
                let mut remote_ready = None;
                let destination5 = destination.cast::<ID3D11Device5>()?;
                destination5.OpenSharedFence(shared.0, &mut remote_ready)?;
                let mut consumed = None;
                destination5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut consumed)?;
                slots.push(Arc::new(Slot {
                    source: source_context.clone(),
                    destination: destination_context.clone(),
                    write,
                    read,
                    ready,
                    remote_ready: remote_ready.context("同卡交付源fence")?,
                    consumed: consumed.context("同卡交付读取fence")?,
                    value: AtomicU64::new(0),
                    leased: AtomicBool::new(false),
                    failed: AtomicBool::new(false),
                    _allocation: allocation,
                }));
            }
            Ok(Self { slots, next: 0 })
        }
    }

    pub(super) fn copy(&mut self, frame: &Frame) -> Result<Option<Delivery>> {
        unsafe {
            for slot in &self.slots {
                ensure!(
                    !slot.failed.load(Ordering::Acquire)
                        && slot.consumed.GetCompletedValue() != u64::MAX
                        && slot.ready.GetCompletedValue() != u64::MAX,
                    "同卡交付fence或设备失效"
                );
            }
            let Some(index) = (0..self.slots.len())
                .map(|i| (self.next + i) % self.slots.len())
                .find(|&i| {
                    let slot = &self.slots[i];
                    !slot.leased.load(Ordering::Acquire)
                        && slot.consumed.GetCompletedValue() >= slot.value.load(Ordering::Acquire)
                })
            else {
                return Ok(None);
            };
            self.next = (index + 1) % self.slots.len();
            let slot = &self.slots[index];
            let value = slot
                .value
                .load(Ordering::Acquire)
                .checked_add(1)
                .context("同卡交付fence编号溢出")?;
            slot.value.store(value, Ordering::Release);
            // Poison before submission: any failure must replace the bridge,
            // never reuse a surface with an untracked in-flight write/read.
            slot.failed.store(true, Ordering::Release);
            slot.source.CopyResource(&slot.write, &frame.image);
            let signalled = slot.source.Signal(&slot.ready, value);
            slot.source.Flush();
            signalled?;
            slot.destination.Wait(&slot.remote_ready, value)?;
            slot.failed.store(false, Ordering::Release);
            slot.leased.store(true, Ordering::Release);
            let mut output = frame.clone();
            output.image = slot.read.clone();
            Ok(Some(Delivery {
                frame: output,
                retire: None,
                local: Some(Lease {
                    slot: slot.clone(),
                    value,
                }),
            }))
        }
    }
}
