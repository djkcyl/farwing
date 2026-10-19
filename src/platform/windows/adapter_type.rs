//! Shared read-only WDDM adapter classification (Windows SDK d3dkmthk.h).
use std::ffi::c_void;
use windows::Win32::Foundation::LUID;

#[repr(C)]
struct OpenAdapter {
    luid: LUID,
    handle: u32,
}
#[repr(C)]
struct QueryAdapter {
    handle: u32,
    kind: u32,
    data: *mut c_void,
    size: u32,
}
#[repr(C)]
struct AdapterHandle {
    handle: u32,
}
#[link(name = "gdi32")]
unsafe extern "system" {
    fn D3DKMTOpenAdapterFromLuid(value: *mut OpenAdapter) -> i32;
    fn D3DKMTQueryAdapterInfo(value: *const QueryAdapter) -> i32;
    fn D3DKMTCloseAdapter(value: *const AdapterHandle) -> i32;
}
impl Drop for AdapterHandle {
    fn drop(&mut self) {
        unsafe {
            D3DKMTCloseAdapter(self);
        }
    }
}

pub(super) fn address(luid: LUID) -> Option<[u32; 3]> {
    let mut opened = OpenAdapter { luid, handle: 0 };
    if unsafe { D3DKMTOpenAdapterFromLuid(&mut opened) } < 0 {
        return None;
    }
    let handle = AdapterHandle {
        handle: opened.handle,
    };
    let mut address = [0u32; 3];
    let status = unsafe {
        D3DKMTQueryAdapterInfo(&QueryAdapter {
            handle: handle.handle,
            kind: 6, // KMTQAITYPE_ADAPTERADDRESS
            data: address.as_mut_ptr().cast(),
            size: std::mem::size_of_val(&address) as u32,
        })
    };
    (status >= 0).then_some(address)
}

/// Unknown classification must not silently remove a potentially usable GPU.
pub(super) fn indirect(luid: LUID) -> Option<bool> {
    let mut opened = OpenAdapter { luid, handle: 0 };
    let result = unsafe { D3DKMTOpenAdapterFromLuid(&mut opened) };
    if result < 0 {
        tracing::debug!(
            status = result,
            "encoder adapter classification open failed"
        );
        return None;
    }
    let handle = AdapterHandle {
        handle: opened.handle,
    };
    let mut flags = 0u32;
    let result = unsafe {
        D3DKMTQueryAdapterInfo(&QueryAdapter {
            handle: handle.handle,
            kind: 15, // KMTQAITYPE_ADAPTERTYPE
            data: (&mut flags as *mut u32).cast(),
            size: std::mem::size_of_val(&flags) as u32,
        })
    };
    if result < 0 {
        tracing::debug!(
            status = result,
            "encoder adapter classification query failed"
        );
        return None;
    }
    Some(flags & (1 << 6) != 0) // D3DKMT_ADAPTERTYPE::IndirectDisplayDevice
}
