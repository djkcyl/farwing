//! Read-only Windows HDR capability queries. Never changes system display mode.
use crate::protocol::capability::DisplayCapability;
use windows::Win32::{
    Devices::Display::*,
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS},
};
use windows::{
    Win32::Graphics::Dxgi::{
        Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, CreateDXGIFactory1, IDXGIFactory1,
        IDXGIOutput6,
    },
    core::Interface,
};

pub(crate) fn capabilities(fallback_fps: u32) -> Vec<DisplayCapability> {
    let queried = unsafe { query_active(fallback_fps) };
    match queried {
        Ok(displays) if !displays.is_empty() => displays,
        _ => vec![DisplayCapability {
            id: 0,
            fps: fallback_fps,
            kind: 0,
            hdr: -1,
        }],
    }
}

fn dxgi_outputs() -> Vec<(String, isize, bool)> {
    let mut hdr_outputs = Vec::new();
    if let Ok(factory) = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
        for index in 0..64 {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
                break;
            };
            for index in 0..64 {
                let Ok(output) = (unsafe { adapter.EnumOutputs(index) }) else {
                    break;
                };
                if let Ok(desc) = output
                    .cast::<IDXGIOutput6>()
                    .and_then(|output| unsafe { output.GetDesc1() })
                {
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    let end = desc
                        .DeviceName
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(desc.DeviceName.len());
                    hdr_outputs.push((
                        String::from_utf16_lossy(&desc.DeviceName[..end]),
                        desc.Monitor.0 as isize,
                        desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                    ));
                }
            }
        }
    }
    hdr_outputs
}

pub(crate) fn monitor_is_hdr(monitor: isize) -> bool {
    // DXGI factories cache display properties. A fresh enumeration avoids
    // stale HDR state after Windows changes advanced-color mode.
    dxgi_outputs()
        .into_iter()
        .any(|(_, id, hdr)| id == monitor && hdr)
}

/// scRGB 1.0 is 80 nits; Windows SDRWhiteLevel uses 1000 for that reference.
/// This is a read-only capture conversion parameter, never a display setting.
pub(crate) fn sdr_white_scale_or(name: &str, fallback: f32) -> f32 {
    unsafe {
        for _ in 0..2 {
            let (mut paths, mut modes) = (0, 0);
            if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes)
                != ERROR_SUCCESS
                || paths > 1024
                || modes > 4096
            {
                break;
            }
            let mut path_data = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
            let mut mode_data = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
            let result = QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut paths,
                path_data.as_mut_ptr(),
                &mut modes,
                mode_data.as_mut_ptr(),
                None,
            );
            if result == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if result != ERROR_SUCCESS {
                break;
            }
            for path in &path_data[..paths as usize] {
                let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                        size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                        adapterId: path.sourceInfo.adapterId,
                        id: path.sourceInfo.id,
                    },
                    ..Default::default()
                };
                if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                    continue;
                }
                let end = source
                    .viewGdiDeviceName
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(source.viewGdiDeviceName.len());
                if String::from_utf16_lossy(&source.viewGdiDeviceName[..end]) != name {
                    continue;
                }
                let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                        size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                        adapterId: path.targetInfo.adapterId,
                        id: path.targetInfo.id,
                    },
                    ..Default::default()
                };
                if DisplayConfigGetDeviceInfo(&mut white.header) == 0 && white.SDRWhiteLevel > 0 {
                    return white.SDRWhiteLevel as f32 / 1000.0;
                }
            }
            break;
        }
    }
    fallback
}

unsafe fn query_active(fallback_fps: u32) -> Result<Vec<DisplayCapability>, ()> {
    let hdr_outputs = dxgi_outputs()
        .into_iter()
        .map(|(name, _, hdr)| (name, hdr))
        .collect::<std::collections::BTreeMap<_, _>>();
    // Buffer sizes can change once between the two Windows enumeration calls.
    for _ in 0..2 {
        let (mut paths, mut modes) = (0, 0);
        if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes) }
            != ERROR_SUCCESS
            || paths > 1024
            || modes > 4096
        {
            return Err(());
        }
        let mut path_data = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
        let mut mode_data = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
        let result = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut paths,
                path_data.as_mut_ptr(),
                &mut modes,
                mode_data.as_mut_ptr(),
                None,
            )
        };
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if result != ERROR_SUCCESS {
            return Err(());
        }
        path_data.truncate(paths as usize);
        return Ok(path_data
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                        size: std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
                        adapterId: path.targetInfo.adapterId,
                        id: path.targetInfo.id,
                    },
                    ..Default::default()
                };
                let queried = unsafe { DisplayConfigGetDeviceInfo(&mut info.header) } == 0;
                let flags = if queried {
                    unsafe { info.Anonymous.value }
                } else {
                    0
                };
                let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                        size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                        adapterId: path.sourceInfo.adapterId,
                        id: path.sourceInfo.id,
                    },
                    ..Default::default()
                };
                let source_ok = unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } == 0;
                let end = source
                    .viewGdiDeviceName
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(source.viewGdiDeviceName.len());
                let hdr = source_ok
                    && hdr_outputs
                        .get(&String::from_utf16_lossy(&source.viewGdiDeviceName[..end]))
                        .copied()
                        .unwrap_or(false);
                let rate = path.targetInfo.refreshRate;
                let fps = if rate.Denominator != 0 {
                    ((rate.Numerator as f64 / rate.Denominator as f64).round() as u32).max(1)
                } else {
                    fallback_fps
                };
                DisplayCapability {
                    id: index as i32,
                    fps,
                    kind: 0,
                    // 0 requires actual HDR10 output. AdvancedColorEnabled
                    // alone can mean WCG/ACM on Windows 11, and is not sufficient.
                    hdr: if hdr {
                        0
                    } else if flags & 1 != 0 && flags & 2 == 0 {
                        -2
                    } else {
                        -1
                    },
                }
            })
            .collect());
    }
    Err(())
}
