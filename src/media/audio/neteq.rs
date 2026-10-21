use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{Result, ensure};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Statistics {
    pub output_samples: u64,
    pub concealed_samples: u64,
    pub inserted_samples: u64,
    pub removed_samples: u64,
    pub discarded_packets: u64,
    pub buffer_ms: u32,
    pub target_ms: u32,
}

unsafe extern "C" {
    fn ou_neteq_create() -> *mut c_void;
    fn ou_neteq_destroy(receiver: *mut c_void);
    fn ou_neteq_insert(
        receiver: *mut c_void,
        payload: *const u8,
        length: usize,
        timestamp: u32,
        sequence: u16,
        age_us: u32,
    ) -> i32;
    fn ou_neteq_set_minimum_delay(receiver: *mut c_void, delay_ms: i32) -> i32;
    fn ou_neteq_audio(
        receiver: *mut c_void,
        output: *mut f32,
        length: usize,
        stats: *mut Statistics,
    ) -> i32;
}

pub(crate) struct Receiver(NonNull<c_void>);
// One owner on the audio callback thread; no pointers or sample slices escape.
unsafe impl Send for Receiver {}

impl Receiver {
    pub fn new() -> Result<Self> {
        NonNull::new(unsafe { ou_neteq_create() })
            .map(Self)
            .ok_or_else(|| anyhow::anyhow!("{}", crate::i18n::t().session.neteq_init_failed))
    }

    /// Insert a packet that arrived at `arrived`; NetEq's jitter estimate
    /// uses that time, not the moment the queue is drained.
    pub fn insert(
        &mut self,
        data: &[u8],
        timestamp: u32,
        sequence: u16,
        arrived: std::time::Instant,
    ) -> bool {
        let age = u32::try_from(arrived.elapsed().as_micros()).unwrap_or(u32::MAX);
        unsafe {
            ou_neteq_insert(
                self.0.as_ptr(),
                data.as_ptr(),
                data.len(),
                timestamp,
                sequence,
                age,
            ) == 0
        }
    }

    /// Keep at least `delay_ms` of audio buffered (cinema mode, to stay in
    /// step with the delayed video); 0 restores lowest latency.
    pub fn set_minimum_delay(&mut self, delay_ms: u32) -> bool {
        let delay = i32::try_from(delay_ms.min(10_000)).unwrap_or(0);
        unsafe { ou_neteq_set_minimum_delay(self.0.as_ptr(), delay) == 0 }
    }

    pub fn block(
        &mut self,
        output: &mut [f32; super::BLOCK * 2],
        stats: &mut Statistics,
    ) -> Result<()> {
        let result =
            unsafe { ou_neteq_audio(self.0.as_ptr(), output.as_mut_ptr(), output.len(), stats) };
        ensure!(result >= 0, "{}", (crate::i18n::t().session.neteq_output_failed)(result));
        if result == 1 {
            tracing::debug!("NetEq returned a silent block after a recoverable decode error");
        }
        Ok(())
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        unsafe { ou_neteq_destroy(self.0.as_ptr()) };
    }
}
