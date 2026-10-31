//! Picture adjustments through a camera's `IAMVideoProcAmp` and
//! `IAMCameraControl`. A camera being captured is adjusted through its open
//! source; another is opened for the call.
use super::reader::SharedSource;
use super::{Apartment, SYNTHETIC, devices};
use crate::i18n::t;
use crate::media::camera::{Control, ControlState};
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, Weak};
use windows::Win32::Media::DirectShow::{IAMCameraControl, IAMVideoProcAmp};
use windows::Win32::Media::MediaFoundation::IMFMediaSource;
use windows::core::Interface;

/// The sources of the cameras being captured, by camera ID.
static OPEN: Mutex<Vec<(String, Weak<SharedSource>)>> = Mutex::new(Vec::new());

fn open() -> std::sync::MutexGuard<'static, Vec<(String, Weak<SharedSource>)>> {
    OPEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// A capture's source in [`OPEN`] while it lives.
pub(super) struct Registration {
    camera: String,
    _source: Arc<SharedSource>,
}

pub(super) fn register(camera: &str, source: Arc<SharedSource>) -> Registration {
    let mut open = open();
    open.retain(|(_, source)| source.strong_count() > 0);
    open.push((camera.to_owned(), Arc::downgrade(&source)));
    Registration {
        camera: camera.to_owned(),
        _source: source,
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let camera = &self.camera;
        open().retain(|(id, source)| id != camera && source.strong_count() > 0);
    }
}

fn with_source<T>(camera: &str, call: impl FnOnce(&IMFMediaSource) -> Result<T>) -> Result<T> {
    let _apartment = Apartment::enter()?;
    let running = open()
        .iter()
        .filter(|(id, _)| id == camera)
        .find_map(|(_, source)| source.upgrade());
    if let Some(source) = running {
        return call(&source.0);
    }
    let (activate, _, _) = devices()?
        .into_iter()
        .find(|(_, id, _)| id == camera)
        .context(t().camera.not_found)?;
    let source: IMFMediaSource = unsafe { activate.ActivateObject() }.map_err(super::failure)?;
    let result = call(&source);
    unsafe {
        let _ = source.Shutdown();
        let _ = activate.ShutdownObject();
    }
    result
}

enum Property {
    ProcAmp(i32),
    Camera(i32),
}

fn property(control: Control) -> Property {
    match control {
        Control::Brightness => Property::ProcAmp(0),
        Control::Contrast => Property::ProcAmp(1),
        Control::Hue => Property::ProcAmp(2),
        Control::Saturation => Property::ProcAmp(3),
        Control::Sharpness => Property::ProcAmp(4),
        Control::Gamma => Property::ProcAmp(5),
        Control::WhiteBalance => Property::ProcAmp(7),
        Control::BacklightCompensation => Property::ProcAmp(8),
        Control::Gain => Property::ProcAmp(9),
        // KSPROPERTY_VIDEOPROCAMP_POWERLINE_FREQUENCY, passed through.
        Control::PowerLineFrequency => Property::ProcAmp(13),
        Control::Exposure => Property::Camera(4),
        Control::Focus => Property::Camera(6),
        Control::Zoom => Property::Camera(3),
    }
}

/// `VideoProcAmp_Flags_Auto` and `CameraControl_Flags_Auto`.
const AUTO: i32 = 1;
const MANUAL: i32 = 2;

fn read(
    control: Control,
    amp: Option<&IAMVideoProcAmp>,
    camera: Option<&IAMCameraControl>,
) -> Option<ControlState> {
    let (mut minimum, mut maximum, mut step, mut default, mut caps) = (0, 0, 0, 0, 0);
    let (mut value, mut flags) = (0, 0);
    unsafe {
        match property(control) {
            Property::ProcAmp(id) => {
                let amp = amp?;
                amp.GetRange(id, &mut minimum, &mut maximum, &mut step, &mut default, &mut caps)
                    .ok()?;
                amp.Get(id, &mut value, &mut flags).ok()?;
            }
            Property::Camera(id) => {
                let camera = camera?;
                camera
                    .GetRange(id, &mut minimum, &mut maximum, &mut step, &mut default, &mut caps)
                    .ok()?;
                camera.Get(id, &mut value, &mut flags).ok()?;
            }
        }
    }
    (maximum > minimum).then_some(ControlState {
        control,
        minimum,
        maximum,
        step: step.max(1),
        default,
        value,
        can_auto: caps & AUTO != 0,
        auto: flags & AUTO != 0,
    })
}

/// The adjustments of an open source, with their current values.
pub(super) fn read_all(source: &IMFMediaSource) -> Vec<ControlState> {
    let amp = source.cast::<IAMVideoProcAmp>().ok();
    let control = source.cast::<IAMCameraControl>().ok();
    Control::ALL
        .into_iter()
        .filter_map(|c| read(c, amp.as_ref(), control.as_ref()))
        .collect()
}

/// The adjustments `camera` supports, with their current values.
pub(crate) fn controls(camera: &str) -> Result<Vec<ControlState>> {
    if camera == SYNTHETIC {
        return Ok(Vec::new());
    }
    with_source(camera, |source| Ok(read_all(source)))
}

/// Set an adjustment of `camera`; `auto` hands it to the camera.
pub(crate) fn set_control(camera: &str, control: Control, value: i32, auto: bool) -> Result<()> {
    let flags = if auto { AUTO } else { MANUAL };
    with_source(camera, |source| unsafe {
        match property(control) {
            Property::ProcAmp(id) => source.cast::<IAMVideoProcAmp>()?.Set(id, value, flags)?,
            Property::Camera(id) => source.cast::<IAMCameraControl>()?.Set(id, value, flags)?,
        }
        Ok(())
    })
}
