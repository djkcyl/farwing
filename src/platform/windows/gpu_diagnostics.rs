//! Opt-in GPU timestamp intervals. These include scheduling gaps, not just execution.
use anyhow::{Context, Result};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use windows::{
    Win32::Graphics::{Direct3D11::*, Dxgi::*},
    core::{HRESULT, Interface},
};

struct Queries {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
}
pub(crate) struct Timing {
    context: ID3D11DeviceContext,
    pending: VecDeque<Queries>,
    idle: Vec<Queries>,
    label: &'static str,
    reported: Instant,
    count: u64,
    total: f64,
    maximum: f64,
}
pub(crate) struct Span<'a> {
    timer: &'a mut Timing,
    queries: Option<Queries>,
}
impl Timing {
    pub fn new(device: &ID3D11Device, label: &'static str) -> Option<Self> {
        if std::env::var_os("FARWING_GPU_TIMING").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return None;
        }
        let result = (|| -> Result<Self> {
            let query = |kind| -> Result<ID3D11Query> {
                let mut result = None;
                unsafe {
                    device.CreateQuery(
                        &D3D11_QUERY_DESC {
                            Query: kind,
                            MiscFlags: 0,
                        },
                        Some(&mut result),
                    )?;
                }
                result.context("GPU timestamp query")
            };
            let mut idle = Vec::new();
            for _ in 0..8 {
                idle.push(Queries {
                    disjoint: query(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                    start: query(D3D11_QUERY_TIMESTAMP)?,
                    end: query(D3D11_QUERY_TIMESTAMP)?,
                });
            }
            Ok(Self {
                context: unsafe { device.GetImmediateContext()? },
                pending: VecDeque::new(),
                idle,
                label,
                reported: Instant::now(),
                count: 0,
                total: 0.,
                maximum: 0.,
            })
        })();
        match result {
            Ok(timer) => Some(timer),
            Err(error) => {
                tracing::debug!(%error, label, "GPU timestamps unavailable");
                None
            }
        }
    }
    pub fn begin(&mut self) -> Option<Span<'_>> {
        self.poll();
        let queries = self.idle.pop()?;
        unsafe {
            self.context.Begin(&queries.disjoint);
            self.context.End(&queries.start);
        }
        Some(Span {
            timer: self,
            queries: Some(queries),
        })
    }
    fn poll(&mut self) {
        while let Some(q) = self.pending.front() {
            let mut clock = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
            let (mut start, mut end) = (0u64, 0u64);
            // The generated GetData wrapper accepts S_FALSE as success. Read
            // the exact HRESULT so incomplete queries never become zero times.
            let ready = unsafe {
                data(&self.context, &q.disjoint, &mut clock)
                    .and_then(|done| {
                        if done {
                            data(&self.context, &q.start, &mut start)
                        } else {
                            Ok(false)
                        }
                    })
                    .and_then(|done| {
                        if done {
                            data(&self.context, &q.end, &mut end)
                        } else {
                            Ok(false)
                        }
                    })
            };
            match ready {
                Ok(false) => break,
                Ok(true) => {
                    if !clock.Disjoint.as_bool() && clock.Frequency > 0 && end >= start {
                        let ms = (end - start) as f64 * 1000. / clock.Frequency as f64;
                        self.count += 1;
                        self.total += ms;
                        self.maximum = self.maximum.max(ms);
                    }
                    self.idle.push(self.pending.pop_front().unwrap());
                }
                Err(error) => {
                    tracing::debug!(%error, stage=self.label, "GPU timestamp query failed");
                    self.pending.clear();
                    self.idle.clear();
                    break;
                }
            }
        }
        if self.reported.elapsed() >= Duration::from_secs(5) {
            if self.count > 0 {
                tracing::info!(
                    stage = self.label,
                    samples = self.count,
                    gpu_interval_avg_ms = self.total / self.count as f64,
                    gpu_interval_max_ms = self.maximum,
                    pending = self.pending.len(),
                    "GPU timestamp interval (includes scheduling gaps)"
                );
            }
            if let Ok(device) = unsafe { self.context.GetDevice() } {
                memory(&device, self.label);
            }
            self.count = 0;
            self.total = 0.;
            self.maximum = 0.;
            self.reported = Instant::now();
        }
    }
}
impl Drop for Span<'_> {
    fn drop(&mut self) {
        if let Some(q) = self.queries.take() {
            unsafe {
                self.timer.context.End(&q.end);
                self.timer.context.End(&q.disjoint);
            }
            self.timer.pending.push_back(q);
        }
    }
}
unsafe fn data<T>(
    context: &ID3D11DeviceContext,
    query: &ID3D11Query,
    output: &mut T,
) -> windows::core::Result<bool> {
    let status = unsafe {
        (context.vtable().GetData)(
            context.as_raw(),
            query.as_raw(),
            (output as *mut T).cast(),
            std::mem::size_of::<T>() as u32,
            D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
        )
    };
    status.ok()?;
    Ok(status == HRESULT(0))
}
pub(crate) fn memory(device: &ID3D11Device, stage: &'static str) {
    let _ = (|| -> Result<()> {
        let adapter: IDXGIAdapter3 =
            unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()? }.cast()?;
        let id = unsafe { adapter.GetDesc()? }.AdapterLuid;
        for (segment, group) in [
            ("local", DXGI_MEMORY_SEGMENT_GROUP_LOCAL),
            ("nonlocal", DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL),
        ] {
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            unsafe {
                adapter.QueryVideoMemoryInfo(0, group, &mut info)?;
            }
            tracing::info!(
                stage,
                adapter = ((id.HighPart as u64) << 32) | id.LowPart as u64,
                node = 0,
                segment,
                budget_bytes = info.Budget,
                process_usage_bytes = info.CurrentUsage,
                "GPU memory budget"
            );
        }
        Ok(())
    })();
}
