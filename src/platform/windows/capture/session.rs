//! Thread-owned capture in the verified SYSTEM session executor.
use super::{Desktop, Frame, Screen};
use crate::platform::windows::{host_service::process, input::system};
use anyhow::{Result, ensure};

pub(super) struct Session {
    // Release all desktop-bound objects before restoring the thread desktop.
    capture: Option<Box<Desktop>>,
    context: system::Desktop,
    session: u32,
    selected: Screen,
    epoch: u64,
    native_epoch: u64,
    pub device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    pub screen: Screen,
    pub available: bool,
    pub cursor: Option<crate::platform::windows::cursor_shape::Snapshot>,
    pub hdr: bool,
    pub backend: &'static str,
}
impl Session {
    pub fn new(selected: &Screen) -> Result<Self> {
        let session = process::session(std::process::id())?;
        ensure!(
            session == process::active_session(),
            "采集Windows会话已结束"
        );
        let mut context = system::Desktop::new()?;
        if let Some((desktop, name)) = context.changed()? {
            context.switch(desktop, name)?;
        }
        let capture = Box::new(Desktop::open_local(selected)?);
        Ok(Self {
            device: capture.device.clone(),
            screen: capture.screen.clone(),
            available: true,
            cursor: None,
            hdr: capture.hdr_available(),
            backend: capture.backend_name(),
            capture: Some(capture),
            context,
            session,
            selected: selected.clone(),
            epoch: 0,
            native_epoch: 0,
        })
    }
    pub fn current(&self) -> &Desktop {
        self.capture.as_ref().expect("active session capture")
    }
    pub fn generation(&self) -> u64 {
        self.epoch
    }
    pub fn next(
        &mut self,
        timeout: u32,
        quality: i32,
        cursor: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        ensure!(
            self.session == process::active_session(),
            "采集Windows会话已结束"
        );
        match self.acquire(timeout, quality, cursor, hdr, maximum) {
            Ok(frame) => Ok(frame),
            Err(error) if error.is::<crate::media::capture::SourceGone>() => Err(error),
            Err(error) => {
                tracing::debug!(error=%format!("{error:#}"), "session capture temporarily unavailable");
                self.capture = None;
                self.epoch = self.epoch.wrapping_add(1);
                self.native_epoch = 0;
                self.available = false;
                self.cursor = None;
                std::thread::sleep(std::time::Duration::from_millis(20));
                Ok(None)
            }
        }
    }
    fn acquire(
        &mut self,
        timeout: u32,
        quality: i32,
        cursor: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        if let Some((desktop, name)) = self.context.changed()? {
            self.capture = None;
            self.context.switch(desktop, name)?;
            self.epoch = self.epoch.wrapping_add(1);
            self.native_epoch = 0;
            tracing::info!(
                ordinary = self.context.ordinary(),
                "capture input desktop changed"
            );
        }
        if self.capture.is_none() {
            self.capture = Some(Box::new(Desktop::open_local(&self.selected)?));
        }
        let capture = self.capture.as_mut().unwrap();
        let frame = capture.next(timeout, quality, cursor, hdr, maximum)?;
        if self.native_epoch != capture.generation {
            self.native_epoch = capture.generation;
            self.epoch = self.epoch.wrapping_add(1);
        }
        self.selected = capture.screen.clone();
        self.device = capture.device.clone();
        self.screen = capture.screen.clone();
        self.available = capture.available;
        self.cursor = capture.cursor.clone();
        self.hdr = capture.hdr_available();
        self.backend = capture.backend_name();
        Ok(frame)
    }
}
