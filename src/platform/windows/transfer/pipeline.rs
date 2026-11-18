//! 采集线程到编码线程的同卡共享纹理环。
//! 上传只在采集线程的立即上下文上提交，编码线程只等待并读取自己的纹理；
//! 两个线程不会同时碰同一个 D3D11 立即上下文。
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

pub(crate) struct Sender {
    context: ID3D11DeviceContext4,
    slots: Vec<Arc<Slot>>,
    next: usize,
}

struct Slot {
    destination: ID3D11DeviceContext4,
    write: ID3D11Texture2D,
    read: ID3D11Texture2D,
    ready: ID3D11Fence,
    remote_ready: ID3D11Fence,
    consumed: ID3D11Fence,
    value: AtomicU64,
    leased: AtomicBool,
    /// 上一次投递因为通道满/取消没有走到编码线程。源纹理的 GPU 写入在
    /// 同一个上下文上按顺序执行，下一次 CopyResource 不会抢在旧写入前面，
    /// 所以没有读取过的槽不需要等 destination 的 consumed fence。
    abandoned: AtomicBool,
    failed: AtomicBool,
    _allocation: ID3D12Resource,
}

pub(crate) struct FrameLease {
    slot: Arc<Slot>,
    value: u64,
    waited: bool,
}
// D3D11 device 是自由线程的；这里移动的是命令提交之后才可能被编码线程读取的
// 槽位，持有者只会在编码线程调用 wait/encode。采集线程只接触 source context。
unsafe impl Send for FrameLease {}

impl Sender {
    pub(crate) fn new(
        source: &ID3D11Device,
        destination: &ID3D11Device,
        desc: D3D11_TEXTURE2D_DESC,
    ) -> Result<Self> {
        ensure!(
            desc.MipLevels == 1 && desc.ArraySize == 1 && desc.SampleDesc.Count == 1,
            "采集交付需要单层非多重采样纹理"
        );
        unsafe {
            let adapter = source.cast::<IDXGIDevice>()?.GetAdapter()?;
            let mut allocator = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut allocator)?;
            let allocator: ID3D12Device = allocator.context("采集交付D3D12设备")?;
            let context: ID3D11DeviceContext4 = source.GetImmediateContext()?.cast()?;
            let destination5 = destination.cast::<ID3D11Device5>()?;
            let mut slots = Vec::with_capacity(3);
            for _ in 0..3 {
                let (allocation, write) = interop(
                    &allocator,
                    source,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                        Width: desc.Width.into(),
                        Height: desc.Height,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        Format: desc.Format,
                        SampleDesc: desc.SampleDesc,
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
                destination5.OpenSharedFence(shared.0, &mut remote_ready)?;
                let mut consumed = None;
                destination5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut consumed)?;
                slots.push(Arc::new(Slot {
                    destination: destination.GetImmediateContext()?.cast()?,
                    write,
                    read,
                    ready,
                    remote_ready: remote_ready.context("采集交付目的fence")?,
                    consumed: consumed.context("采集交付读取fence")?,
                    value: AtomicU64::new(0),
                    leased: AtomicBool::new(false),
                    abandoned: AtomicBool::new(false),
                    failed: AtomicBool::new(false),
                    _allocation: allocation,
                }));
            }
            Ok(Self {
                context,
                slots,
                next: 0,
            })
        }
    }

    /// 直接把 DXGI 刚抓到的纹理复制进共享环，省掉一次中间纹理。
    pub(crate) fn send_texture(&mut self, image: &ID3D11Texture2D) -> Result<Option<FrameLease>> {
        unsafe {
            ensure!(
                !self
                    .slots
                    .iter()
                    .any(|slot| slot.failed.load(Ordering::Acquire)),
                "采集交付fence失效"
            );
            let Some(index) = (0..self.slots.len())
                .map(|i| (self.next + i) % self.slots.len())
                .find(|&i| {
                    let slot = &self.slots[i];
                    !slot.leased.load(Ordering::Acquire)
                        && (slot.abandoned.load(Ordering::Acquire)
                            || slot.consumed.GetCompletedValue()
                                >= slot.value.load(Ordering::Acquire))
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
                .context("采集交付fence编号溢出")?;
            slot.value.store(value, Ordering::Release);
            slot.leased.store(true, Ordering::Release);
            slot.failed.store(true, Ordering::Release);
            slot.abandoned.store(false, Ordering::Release);
            self.context.CopyResource(&slot.write, image);
            let signalled = self.context.Signal(&slot.ready, value);
            self.context.Flush();
            signalled?;
            slot.failed.store(false, Ordering::Release);
            Ok(Some(FrameLease {
                slot: slot.clone(),
                value,
                waited: false,
            }))
        }
    }
}

impl FrameLease {
    pub(crate) fn wait(&mut self) -> Result<()> {
        ensure!(!self.waited, "采集交付重复等待");
        unsafe {
            self.slot
                .destination
                .Wait(&self.slot.remote_ready, self.value)?;
        }
        self.waited = true;
        Ok(())
    }

    pub(crate) fn texture(&self) -> &ID3D11Texture2D {
        &self.slot.read
    }
}

impl Drop for FrameLease {
    fn drop(&mut self) {
        unsafe {
            if self.waited {
                if self
                    .slot
                    .destination
                    .Signal(&self.slot.consumed, self.value)
                    .is_err()
                {
                    self.slot.failed.store(true, Ordering::Release);
                }
                self.slot.destination.Flush();
            } else {
                self.slot.abandoned.store(true, Ordering::Release);
            }
            self.slot.leased.store(false, Ordering::Release);
        }
    }
}
