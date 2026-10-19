//! Default-change callbacks record intent; the audio owner applies all changes.
use super::defaults::{Scope, User};
use anyhow::Result;
use crossbeam_queue::ArrayQueue;
use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::{
    Win32::{Foundation::PROPERTYKEY, Media::Audio::*, System::Com::*},
    core::{PCWSTR, implement},
};
pub(super) struct Change {
    pub flow: i32,
    pub role: i32,
    pub id: Option<String>,
}
struct Notices {
    queue: ArrayQueue<Change>,
    overflow: AtomicBool,
}
#[implement(IMMNotificationClient)]
struct Callback(Arc<Notices>);
impl IMMNotificationClient_Impl for Callback_Impl {
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        id: &PCWSTR,
    ) -> windows::core::Result<()> {
        let id = if id.is_null() {
            None
        } else {
            unsafe { id.to_string().ok() }
        };
        if self
            .0
            .queue
            .force_push(Change {
                flow: flow.0,
                role: role.0,
                id,
            })
            .is_some()
        {
            self.0.overflow.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}
pub(super) struct Watch {
    enumerator: IMMDeviceEnumerator,
    callback: IMMNotificationClient,
    notices: Arc<Notices>,
    user: Rc<User>,
    _apartment: Scope,
}
impl Watch {
    pub fn new(user: Rc<User>) -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let mut apartment = Scope {
            impersonated: false,
            apartment: false,
        };
        if hr.0 != 0x80010106u32 as i32 {
            hr.ok()?;
            apartment.apartment = true;
        }
        let notices = Arc::new(Notices {
            queue: ArrayQueue::new(64),
            overflow: AtomicBool::new(false),
        });
        let callback: IMMNotificationClient = Callback(notices.clone()).into();
        let enumerator = user.with(|e, _| {
            unsafe { e.RegisterEndpointNotificationCallback(&callback)? };
            Ok(e)
        })?;
        Ok(Self {
            enumerator,
            callback,
            notices,
            user,
            _apartment: apartment,
        })
    }
    pub fn drain(&self) -> (bool, Vec<Change>) {
        let mut changes = Vec::new();
        while let Some(change) = self.notices.queue.pop() {
            changes.push(change)
        }
        (self.notices.overflow.swap(false, Ordering::AcqRel), changes)
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.user.with(|_, _| {
            unsafe {
                self.enumerator
                    .UnregisterEndpointNotificationCallback(&self.callback)?
            };
            Ok(())
        });
    }
}
