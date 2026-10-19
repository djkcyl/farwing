//! Local-only, bounded and cancellable transport for the privileged input owner.
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Foundation::*,
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::{IO::*, Pipes::*, Threading::*},
    },
    core::{PCWSTR, w},
};

pub(crate) struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
pub(crate) struct Pipe {
    pub handle: Handle,
    reader: std::cell::RefCell<Option<Box<Prefix>>>,
}
// A pipe has one exclusive owner. Pending read buffers/OVERLAPPED are boxed and
// use events, so moving ownership does not move kernel-referenced memory. It is
// deliberately not Sync: requests on one connection remain serialized.
unsafe impl Send for Pipe {}
struct Prefix {
    bytes: [u8; 4],
    offset: usize,
    op: OVERLAPPED,
    event: Handle,
    pending: bool,
}
impl Drop for Pipe {
    fn drop(&mut self) {
        if let Some(prefix) = self.reader.get_mut().as_mut()
            && prefix.pending
        {
            unsafe {
                let _ = CancelIoEx(self.handle.0, Some(&prefix.op));
                let mut n = 0;
                let _ = GetOverlappedResult(self.handle.0, &prefix.op, &mut n, true);
            }
        }
    }
}
pub(crate) static NAME: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::pipe_name("Input.v1"));
const MAX_FRAME: usize = 6 * 1024 * 1024;
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
impl Pipe {
    pub fn server(name: &str, interactive: bool) -> Result<Self> {
        Self::listener(name, interactive, 1, true)
    }
    pub fn listener(name: &str, interactive: bool, instances: u32, first: bool) -> Result<Self> {
        unsafe {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                if interactive {
                    w!("D:P(A;;GA;;;SY)(A;;GRGW;;;IU)")
                } else {
                    w!("D:P(A;;GA;;;SY)")
                },
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )?;
            let security = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            };
            let handle = CreateNamedPipeW(
                PCWSTR(wide(name).as_ptr()),
                PIPE_ACCESS_DUPLEX
                    | FILE_FLAG_OVERLAPPED
                    | if first {
                        FILE_FLAG_FIRST_PIPE_INSTANCE
                    } else {
                        FILE_FLAGS_AND_ATTRIBUTES(0)
                    },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                instances,
                64 * 1024,
                64 * 1024,
                0,
                Some(&security),
            );
            let error = windows::core::Error::from_thread();
            LocalFree(Some(HLOCAL(descriptor.0)));
            if handle.is_invalid() {
                return Err(error.into());
            }
            Ok(Self {
                handle: Handle(handle),
                reader: Default::default(),
            })
        }
    }
    pub fn client(name: &str) -> Result<Option<Self>> {
        match unsafe {
            CreateFileW(
                PCWSTR(wide(name).as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        } {
            Ok(handle) => Ok(Some(Self {
                handle: Handle(handle),
                reader: Default::default(),
            })),
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    /// Wait for `op`; `permitted` and `deadline` are looked at every `poll`
    /// milliseconds (completion itself wakes the wait at once).
    fn pending(
        &self,
        op: &mut OVERLAPPED,
        deadline: Instant,
        permitted: &impl Fn() -> bool,
        poll: u32,
    ) -> Result<u32> {
        loop {
            let wait = unsafe { WaitForSingleObject(op.hEvent, poll) };
            if wait == WAIT_OBJECT_0 {
                let mut count = 0;
                unsafe {
                    GetOverlappedResult(self.handle.0, op, &mut count, false)?;
                }
                return Ok(count);
            }
            if wait != WAIT_TIMEOUT || !permitted() || Instant::now() >= deadline {
                unsafe {
                    let _ = CancelIoEx(self.handle.0, Some(op));
                    // Drain cancellation before the kernel can reference stack data.
                    let mut count = 0;
                    let _ = GetOverlappedResult(self.handle.0, op, &mut count, true);
                }
                anyhow::bail!("被控服务通信已取消或超时");
            }
        }
    }
    pub fn accept(&self, permitted: impl Fn() -> bool) -> Result<()> {
        let event = Handle(unsafe { CreateEventW(None, true, false, None)? });
        let mut op = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        match unsafe { ConnectNamedPipe(self.handle.0, Some(&mut op)) } {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(()),
            // A listener waits for its next client most of the time: look at
            // `permitted` (shutdown, deadlines) less often than during I/O.
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => self
                .pending(
                    &mut op,
                    Instant::now() + Duration::from_secs(86400),
                    &permitted,
                    100,
                )
                .map(|_| ()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn peer_pid(&self, server: bool) -> Result<u32> {
        let mut pid = 0;
        unsafe {
            if server {
                GetNamedPipeClientProcessId(self.handle.0, &mut pid)?
            } else {
                GetNamedPipeServerProcessId(self.handle.0, &mut pid)?
            }
        };
        Ok(pid)
    }
    pub fn peer_session(&self, server: bool) -> Result<u32> {
        let mut session = 0;
        unsafe {
            if server {
                GetNamedPipeClientSessionId(self.handle.0, &mut session)?;
            } else {
                GetNamedPipeServerSessionId(self.handle.0, &mut session)?;
            }
        }
        Ok(session)
    }
    pub fn queued_bytes(&self) -> Result<u32> {
        let mut count = 0;
        unsafe {
            PeekNamedPipe(self.handle.0, None, 0, None, Some(&mut count), None)?;
        }
        Ok(count)
    }
    pub fn available(&self) -> Result<bool> {
        self.available_within(5)
    }
    /// Whether a message has started to arrive, waiting up to `wait`
    /// milliseconds (an arriving message ends the wait at once). Loops that
    /// only serve requests wait longer, and so wake less often while idle.
    pub fn available_within(&self, wait: u32) -> Result<bool> {
        let mut reader = self.reader.borrow_mut();
        if reader.is_none() {
            let event = Handle(unsafe { CreateEventW(None, true, false, None)? });
            *reader = Some(Box::new(Prefix {
                bytes: [0; 4],
                offset: 0,
                op: OVERLAPPED {
                    hEvent: event.0,
                    ..Default::default()
                },
                event,
                pending: false,
            }));
        }
        let prefix = reader.as_mut().unwrap();
        if prefix.offset == 4 {
            return Ok(true);
        }
        let mut count = 0;
        if !prefix.pending {
            unsafe {
                ResetEvent(prefix.event.0)?;
            }
            prefix.op = OVERLAPPED {
                hEvent: prefix.event.0,
                ..Default::default()
            };
            let offset = prefix.offset;
            let prefix = &mut **prefix;
            match unsafe {
                ReadFile(
                    self.handle.0,
                    Some(&mut prefix.bytes[offset..]),
                    Some(&mut count),
                    Some(&mut prefix.op),
                )
            } {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => prefix.pending = true,
                Err(e) => return Err(e.into()),
            }
        }
        if prefix.pending {
            let wait = unsafe { WaitForSingleObject(prefix.event.0, wait) };
            if wait == WAIT_TIMEOUT {
                return Ok(false);
            }
            ensure!(wait == WAIT_OBJECT_0, "被控服务等待失败");
            // The kernel has completed the buffer before this state is cleared.
            prefix.pending = false;
            unsafe {
                GetOverlappedResult(self.handle.0, &prefix.op, &mut count, false)?;
            }
        }
        ensure!(count > 0, "被控服务已断开");
        prefix.offset += count as usize;
        Ok(prefix.offset == 4)
    }
    fn transfer(&self, bytes: &mut [u8], write: bool, permitted: &impl Fn() -> bool) -> Result<()> {
        self.transfer_until(
            bytes,
            write,
            permitted,
            Instant::now() + Duration::from_millis(500),
        )
    }
    fn transfer_until(
        &self,
        bytes: &mut [u8],
        write: bool,
        permitted: &impl Fn() -> bool,
        deadline: Instant,
    ) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            ensure!(permitted(), "输入许可已撤销");
            let event = Handle(unsafe { CreateEventW(None, true, false, None)? });
            let mut op = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            let mut count = 0;
            let result = unsafe {
                if write {
                    WriteFile(
                        self.handle.0,
                        Some(&bytes[offset..]),
                        Some(&mut count),
                        Some(&mut op),
                    )
                } else {
                    ReadFile(
                        self.handle.0,
                        Some(&mut bytes[offset..]),
                        Some(&mut count),
                        Some(&mut op),
                    )
                }
            };
            match result {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                    count = self.pending(&mut op, deadline, permitted, 5)?
                }
                Err(e) => return Err(e.into()),
            }
            ensure!(count > 0, "被控服务已断开");
            offset += count as usize;
        }
        Ok(())
    }
    pub fn send<T: serde::Serialize>(
        &self,
        message: &T,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        self.send_raw(serde_json::to_vec(message)?, permitted)
    }
    pub(crate) fn send_raw(&self, mut bytes: Vec<u8>, permitted: impl Fn() -> bool) -> Result<()> {
        ensure!(bytes.len() <= MAX_FRAME, "被控服务消息过长");
        self.transfer(&mut (bytes.len() as u32).to_le_bytes(), true, &permitted)?;
        self.transfer(&mut bytes, true, &permitted)
    }
    pub fn receive<T: serde::de::DeserializeOwned>(
        &self,
        permitted: impl Fn() -> bool,
    ) -> Result<T> {
        self.receive_timeout(Duration::from_millis(500), permitted)
    }
    pub fn receive_timeout<T: serde::de::DeserializeOwned>(
        &self,
        timeout: Duration,
        permitted: impl Fn() -> bool,
    ) -> Result<T> {
        let bytes = self.receive_raw(timeout, permitted)?;
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("被控服务消息无效"))
    }
    pub(crate) fn receive_raw(
        &self,
        timeout: Duration,
        permitted: impl Fn() -> bool,
    ) -> Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut size = [0; 4];
        if self.reader.borrow().is_some() {
            while !self.available()? {
                ensure!(
                    permitted() && Instant::now() < deadline,
                    "被控服务通信已取消或超时"
                );
            }
            let mut reader = self.reader.borrow_mut();
            let prefix = reader.as_mut().unwrap();
            size = prefix.bytes;
            prefix.offset = 0;
        } else {
            self.transfer_until(&mut size, false, &permitted, deadline)?;
        }
        let size = u32::from_le_bytes(size) as usize;
        ensure!(size <= MAX_FRAME, "被控服务消息过长");
        let mut bytes = vec![0; size];
        self.transfer_until(&mut bytes, false, &permitted, deadline)?;
        Ok(bytes)
    }
}
