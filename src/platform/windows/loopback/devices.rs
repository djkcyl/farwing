//! Session-owned endpoint discovery. Callbacks enqueue notices; only the owner
//! thread enumerates devices, rebuilds streams or unregisters COM callbacks.
use super::Apartment;
use anyhow::{Result, ensure};
use crossbeam_queue::ArrayQueue;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, Ordering},
};
use windows::{
    Win32::{
        Foundation::{ERROR_NOT_FOUND, PROPERTYKEY},
        Media::Audio::*,
        System::Com::{
            StructuredStorage::{PropVariantClear, PropVariantToString},
            *,
        },
    },
    core::{BOOL, GUID, Interface, PCWSTR, PWSTR, implement},
};

enum Event {
    Default,
    Device(String),
    Format(String),
}
struct Notices {
    events: ArrayQueue<Event>,
    overflow: AtomicBool,
    owner: std::thread::Thread,
}
impl Notices {
    fn push(&self, event: Event) {
        if self.events.force_push(event).is_some() {
            self.overflow.store(true, Ordering::Release);
        }
        self.owner.unpark();
    }
}
#[implement(IMMNotificationClient)]
struct Notifications(Arc<Notices>);
impl IMMNotificationClient_Impl for Notifications_Impl {
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            self.0.push(Event::Default);
        }
        Ok(())
    }
    fn OnDeviceStateChanged(&self, id: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        self.OnDeviceAdded(id)
    }
    fn OnDeviceAdded(&self, id: &PCWSTR) -> windows::core::Result<()> {
        if !id.is_null() {
            if let Ok(id) = unsafe { id.to_string() } {
                self.0.push(Event::Device(id));
            }
        }
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
        self.OnDeviceAdded(id)
    }
    fn OnPropertyValueChanged(&self, id: &PCWSTR, key: &PROPERTYKEY) -> windows::core::Result<()> {
        if (*key == PKEY_AudioEngine_DeviceFormat || *key == PKEY_AudioEngine_OEMFormat)
            && !id.is_null()
        {
            if let Ok(id) = unsafe { id.to_string() } {
                self.0.push(Event::Format(id));
            }
        }
        Ok(())
    }
}
pub(super) struct TaskString(pub PWSTR);
impl Drop for TaskString {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.0.0.cast()));
        }
    }
}
pub(crate) struct Endpoint {
    pub id: String,
    pub device: IMMDevice,
}
impl Endpoint {
    pub fn name(&self) -> String {
        let read = || -> windows::core::Result<String> {
            unsafe {
                let store = self.device.OpenPropertyStore(STGM_READ)?;
                let mut value = store.GetValue(&PROPERTYKEY {
                    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
                    pid: 14,
                })?;
                let mut text = [0u16; 1024];
                let result = PropVariantToString(&value, &mut text);
                let _ = PropVariantClear(&mut value);
                result?;
                Ok(String::from_utf16_lossy(
                    &text[..text.iter().position(|c| *c == 0).unwrap_or(text.len())],
                ))
            }
        };
        read().unwrap_or_else(|_| "系统默认播放设备".into())
    }
}
#[derive(Default)]
pub(crate) struct Change {
    pub refresh: bool,
    pub rebuild: bool,
}
pub(crate) struct Devices {
    enumerator: IMMDeviceEnumerator,
    callback: IMMNotificationClient,
    notices: Arc<Notices>,
    _apartment: Apartment,
}
impl Devices {
    pub fn new() -> Result<Self> {
        let apartment = Apartment::new()?;
        let notices = Arc::new(Notices {
            events: ArrayQueue::new(32),
            overflow: AtomicBool::new(false),
            owner: std::thread::current(),
        });
        let callback: IMMNotificationClient = Notifications(notices.clone()).into();
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            enumerator.RegisterEndpointNotificationCallback(&callback)?;
            Ok(Self {
                enumerator,
                callback,
                notices,
                _apartment: apartment,
            })
        }
    }
    pub fn changes(&self, selected: Option<&str>) -> Change {
        let overflow = self.notices.overflow.swap(false, Ordering::AcqRel);
        let mut change = Change {
            refresh: overflow,
            rebuild: overflow,
        };
        while let Some(event) = self.notices.events.pop() {
            match event {
                Event::Default => change.refresh = true,
                Event::Device(id) => {
                    change.refresh = true;
                    change.rebuild |= selected == Some(id.as_str());
                }
                Event::Format(id) => {
                    if selected == Some(id.as_str()) {
                        change.refresh = true;
                        change.rebuild = true;
                    }
                }
            }
        }
        change
    }
    pub fn endpoint(&self, selected: Option<&str>) -> Result<Option<Endpoint>> {
        unsafe {
            let device = match selected {
                Some(id) => {
                    let id: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
                    self.enumerator.GetDevice(PCWSTR(id.as_ptr()))
                }
                None => self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole),
            };
            let device = match device {
                Ok(device) => device,
                Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            ensure!(
                device.cast::<IMMEndpoint>()?.GetDataFlow()? == eRender,
                "所选设备不是播放设备"
            );
            if device.GetState()?.0 & DEVICE_STATE_ACTIVE.0 == 0 {
                return Ok(None);
            }
            Ok(Some(Self::describe(device)?))
        }
    }
    fn describe(device: IMMDevice) -> Result<Endpoint> {
        unsafe {
            let id = TaskString(device.GetId()?);
            ensure!(!id.0.is_null(), "音频端点标识为空");
            Ok(Endpoint {
                id: id.0.to_string()?,
                device,
            })
        }
    }
    pub fn list(&self) -> Result<Vec<Endpoint>> {
        unsafe {
            let collection = self
                .enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut devices = Vec::new();
            for i in 0..collection.GetCount()? {
                // A device can disappear between enumeration and property lookup.
                if let Ok(device) = collection
                    .Item(i)
                    .map_err(anyhow::Error::from)
                    .and_then(Self::describe)
                {
                    devices.push(device);
                }
            }
            Ok(devices)
        }
    }
}
impl Drop for Devices {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.callback);
        }
    }
}

// Each stream owns a distinct notice flag, so a late callback from an old
// stream cannot invalidate its replacement. Volume/mute are not disconnections.
#[implement(IAudioSessionEvents)]
struct SessionEvents {
    disconnected: Arc<AtomicI32>,
    owner: std::thread::Thread,
}
impl IAudioSessionEvents_Impl for SessionEvents_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, _: f32, _: BOOL, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnChannelVolumeChanged(
        &self,
        _: u32,
        _: *const f32,
        _: u32,
        _: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _: AudioSessionState) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSessionDisconnected(
        &self,
        reason: AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        self.disconnected.store(reason.0, Ordering::Release);
        self.owner.unpark();
        Ok(())
    }
}
pub(super) struct SessionWatch {
    control: IAudioSessionControl,
    callback: IAudioSessionEvents,
    disconnected: Arc<AtomicI32>,
}
impl SessionWatch {
    pub fn new(client: &IAudioClient) -> Result<Self> {
        let disconnected = Arc::new(AtomicI32::new(-1));
        let callback: IAudioSessionEvents = SessionEvents {
            disconnected: disconnected.clone(),
            owner: std::thread::current(),
        }
        .into();
        let control: IAudioSessionControl = unsafe { client.GetService()? };
        unsafe {
            control.RegisterAudioSessionNotification(&callback)?;
        }
        Ok(Self {
            control,
            callback,
            disconnected,
        })
    }
    pub fn check(&self) -> Result<()> {
        let reason = self.disconnected.load(Ordering::Acquire);
        ensure!(reason < 0, "音频会话已失效（原因 {reason}）");
        Ok(())
    }
}
impl Drop for SessionWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .control
                .UnregisterAudioSessionNotification(&self.callback);
        }
    }
}
