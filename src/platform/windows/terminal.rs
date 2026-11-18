//! Host terminal on Windows, over ConPTY.
//!
//! UU's controllers can open a shell on the controlled machine (connection
//! `kind = 8`). Windows runs it on a pseudo console: `CreatePseudoConsole`
//! gives us a pty, the child is started with
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, and the two pipe ends are ours.
//!
//! Anonymous pipes are blocking, so reads and writes each get a thread and a
//! channel. That keeps `read`/`write` non-blocking for the caller, which the
//! frame layer needs: it polls every session from one tick and must never
//! wait on a single shell.

use anyhow::{Context, Result, bail};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    ResumeThread, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

/// ConPTY 不暴露前台进程组。
pub(crate) fn foreground_supported() -> bool {
    false
}

/// Why this process cannot serve a terminal; `None` when it can.
///
/// The unattended service runs the host as LocalSystem. A shell started from
/// there would hand the controller SYSTEM on a machine whose owner only
/// granted "open a terminal as me", the same widening the POSIX backend
/// refuses for root. An identity that cannot be read is refused too: a shell
/// must not start under an account nobody checked.
pub(crate) fn unavailable() -> Option<&'static str> {
    /// LocalSystem's well-known SID.
    const LOCAL_SYSTEM: &str = "S-1-5-18";
    static REFUSAL: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *REFUSAL.get_or_init(
        || match super::host_service::vault::sid(std::process::id()) {
            Ok(sid) if sid == LOCAL_SYSTEM => {
                Some("以 SYSTEM 身份运行（无人值守模式）时不提供终端")
            }
            Ok(_) => None,
            Err(_) => Some("无法确认运行身份，不提供终端"),
        },
    )
}

/// Maps the controller's shell choice to a program.
///
/// The controller names a shell *kind* (`powershell`, `cmd`), not a path. A
/// kind this machine does not have falls back to the login shell rather than
/// failing the request.
///
/// The result is an absolute path where the shell was found: `CreateProcessW`
/// would search `PATH` for a bare name, but callers and tests compare the
/// answer against the filesystem, and an absolute path also keeps a stray
/// `cmd.exe` earlier in `PATH` from being run instead.
pub(crate) fn resolve_shell(requested: &str) -> String {
    let name = match requested.trim().to_ascii_lowercase().as_str() {
        "powershell" | "powershell.exe" | "pwsh" => "powershell.exe",
        "cmd" | "cmd.exe" => "cmd.exe",
        _ => return default_shell(),
    };
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
    let candidate = match name {
        // PowerShell keeps its own versioned directory, not System32 itself.
        "powershell.exe" => format!("{root}\\System32\\WindowsPowerShell\\v1.0\\{name}"),
        _ => format!("{root}\\System32\\{name}"),
    };
    if std::path::Path::new(&candidate).exists() {
        candidate
    } else {
        name.to_owned()
    }
}

/// `ComSpec`, which is `cmd.exe` unless the user changed it.
pub(crate) fn default_shell() -> String {
    std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_owned())
}

/// The shell and everything it starts, held in one Windows job.
///
/// Windows has no session id to walk the way Linux does, so the tree is kept
/// here instead: `terminate` ends the whole job at once, and
/// `KILL_ON_JOB_CLOSE` means nothing the shell started outlives this process
/// even if it exits without asking.
struct Job(HANDLE);

impl Job {
    fn create() -> Result<Self> {
        // SAFETY: both arguments are optional and the out-parameter is owned.
        let handle = unsafe { CreateJobObjectW(None, None) }.context("创建作业对象失败")?;
        let job = Self(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the job is live and `limits` is exactly the struct the class
        // asks for.
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .context("设置作业对象失败")?;
        Ok(job)
    }

    fn assign(&self, process: HANDLE) -> Result<()> {
        // SAFETY: both handles are live; the process is ours to put in a job
        // as long as it has not already joined one.
        unsafe { AssignProcessToJobObject(self.0, process) }.context("把 shell 放进作业对象失败")
    }

    fn terminate(&self) {
        // SAFETY: the job is live for the life of `self`.
        unsafe {
            let _ = TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // Closing the last handle to the job is what `KILL_ON_JOB_CLOSE`
        // reacts to, so this is also the backstop for an unclean exit.
        // SAFETY: the handle is ours and is not used again.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// How much input may wait for the writer thread, like a pty's input buffer.
/// Past this, `write` takes nothing and the shared layer holds the rest.
const INPUT_QUEUE: usize = 64 * 1024;

/// A handle handed to a worker thread.
///
/// [`HANDLE`] wraps a raw pointer and so is not `Send`; ownership is moved to
/// exactly one thread, which is all these threads need.
struct SendHandle(HANDLE);
// SAFETY: the handle is owned by the thread it moves to and never shared.
unsafe impl Send for SendHandle {}

/// A shell running on a pseudo console.
pub(crate) struct Session {
    console: HPCON,
    process: HANDLE,
    /// Owns the shell and everything it starts; see [`Job`].
    job: Job,
    /// Input queued for the writer thread; `None` once it has stopped.
    input: Mutex<Option<SyncSender<Vec<u8>>>>,
    /// Bytes queued for the writer thread and not yet in the console.
    queued: Arc<AtomicUsize>,
    /// Output the reader thread has produced but the frame layer has not
    /// taken yet.
    output: Mutex<Output>,
    shell: String,
}

// SAFETY: the handles are used through the channels and the console API, not
// shared as pointers, and the process handle is only waited on.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl Session {
    /// Starts `shell` on a new pseudo console of `cols` x `rows`.
    pub(crate) fn open(shell: &str, cols: u16, rows: u16) -> Result<Self> {
        let size = coord(cols, rows);
        // Two anonymous pipes: we write to `input`, the console reads it; the
        // console writes `output`, we read it. `CreatePipe` hands back both
        // ends of each, and the child inherits the ends it needs.
        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;
        // SAFETY: both pairs are live handles from `CreatePipe`; the console
        // takes its own reference to the two ends it is given.
        let console = unsafe {
            CreatePseudoConsole(size, input_read, output_write, 0).context("创建伪控制台失败")?
        };
        // The console holds these; ours would only keep them open.
        // SAFETY: the handles are ours to close and are not used again.
        unsafe {
            let _ = CloseHandle(input_read);
            let _ = CloseHandle(output_write);
        }

        let (process, job) = match spawn(shell, console) {
            Ok(process) => process,
            Err(error) => {
                // SAFETY: the console was created above and is ours.
                unsafe { ClosePseudoConsole(console) };
                // SAFETY: the pipe ends are ours and not used again.
                unsafe {
                    let _ = CloseHandle(input_write);
                    let _ = CloseHandle(output_read);
                }
                return Err(error);
            }
        };

        let (output_tx, output_rx) = sync_channel(64);
        // Wrap before the closure so the thread gets a `Send` value.
        let output_read = SendHandle(output_read);
        std::thread::spawn(move || read_output(output_read, output_tx));
        let (input_tx, input_rx) = sync_channel(64);
        let input_write = SendHandle(input_write);
        let queued = Arc::new(AtomicUsize::new(0));
        let drained = Arc::clone(&queued);
        std::thread::spawn(move || write_input(input_write, input_rx, drained));

        Ok(Self {
            console,
            process,
            job,
            input: Mutex::new(Some(input_tx)),
            queued,
            output: Mutex::new(Output {
                channel: Some(output_rx),
                carry: Vec::new(),
            }),
            shell: shell.to_owned(),
        })
    }

    /// The shell this session runs.
    /// ConPTY 不暴露前台进程组，控制台 API 也不给进程树；这里先不报。
    pub(crate) fn foreground(&self) -> Option<crate::platform::Foreground> {
        None
    }

    pub(crate) fn shell(&self) -> &str {
        &self.shell
    }

    /// Takes whatever the reader thread has buffered.
    ///
    /// `Ok(None)` means nothing is ready; `Ok(Some(0))` is end-of-file, which
    /// is how the reader reports a closed console.
    ///
    /// A chunk larger than `buffer` is handed out over several calls, the way
    /// a pty read returns what fits and leaves the rest queued.
    pub(crate) fn read(&self, buffer: &mut [u8]) -> Result<Option<usize>> {
        if buffer.is_empty() {
            return Ok(None);
        }
        let mut output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        if output.carry.is_empty() {
            let Some(channel) = output.channel.as_ref() else {
                return Ok(Some(0));
            };
            match channel.try_recv() {
                Ok(chunk) => output.carry = chunk,
                Err(TryRecvError::Empty) => return Ok(None),
                // The reader thread ended: the console closed.
                Err(TryRecvError::Disconnected) => return Ok(Some(0)),
            }
        }
        let take = output.carry.len().min(buffer.len());
        buffer[..take].copy_from_slice(&output.carry[..take]);
        output.carry.drain(..take);
        Ok(Some(take))
    }

    /// Queues keyboard input for the writer thread.
    ///
    /// Behaves like a non-blocking pty write: it takes what fits in the
    /// writer's queue and reports how much, and `Ok(0)` when the queue is
    /// full. The caller holds the rest, so the shared layer's own input limit
    /// and its overflow answer apply here as they do on POSIX, and a shell
    /// that stopped reading never blocks the caller, which holds the lock of
    /// every session.
    pub(crate) fn write(&self, data: &[u8]) -> Result<usize> {
        let channel = self.input.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sender) = channel.as_ref() else {
            bail!("伪控制台已关闭");
        };
        let room = INPUT_QUEUE.saturating_sub(self.queued.load(Ordering::Acquire));
        let take = data.len().min(room);
        if take == 0 {
            return Ok(0);
        }
        // Counted before the send so the writer can never subtract first.
        self.queued.fetch_add(take, Ordering::AcqRel);
        match sender.try_send(data[..take].to_vec()) {
            Ok(()) => Ok(take),
            Err(TrySendError::Full(_)) => {
                self.queued.fetch_sub(take, Ordering::AcqRel);
                Ok(0)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.queued.fetch_sub(take, Ordering::AcqRel);
                bail!("伪控制台已关闭")
            }
        }
    }

    /// Applies a new size, which the console reports to the shell.
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let size = coord(cols, rows);
        // SAFETY: the console is live for the life of `self`.
        unsafe { ResizePseudoConsole(self.console, size) }.context("调整伪控制台尺寸失败")
    }

    /// Whether the shell has exited.
    pub(crate) fn exited(&mut self) -> bool {
        // SAFETY: the process handle is open for the life of `self`.
        let waited = unsafe { WaitForSingleObject(self.process, 0) };
        if waited == WAIT_OBJECT_0 {
            return true;
        }
        // A bad handle must not look like a live shell.
        if waited == WAIT_FAILED {
            return true;
        }
        let mut code = 0u32;
        // SAFETY: as above, with a live out-parameter.
        matches!(
            unsafe { GetExitCodeProcess(self.process, &mut code) },
            Ok(())
        ) && code != 259 // STILL_ACTIVE
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The shell and everything it started go first, so nothing is left
        // writing to the console.
        self.job.terminate();
        // SAFETY: the process handle was created in `open` and is owned here.
        unsafe {
            let _ = TerminateProcess(self.process, 1);
        }
        // Then the channels. Before Windows 11 24H2, `ClosePseudoConsole`
        // waits until the console's last output has been read; a reader stuck
        // on a full channel (a detached or backpressured session) would never
        // read it, and the close would hang this thread for good. Without a
        // receiver the reader's next send fails and it closes the pipe, which
        // ends the console's write. Dropping the sender ends the writer.
        let _ = self
            .output
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .channel
            .take();
        let _ = self
            .input
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // SAFETY: as above; neither handle is used again.
        unsafe {
            ClosePseudoConsole(self.console);
            let _ = CloseHandle(self.process);
        }
    }
}

/// A console size. The shared layer already bounds it; this only keeps a
/// value past `i16::MAX` from wrapping negative.
fn coord(cols: u16, rows: u16) -> COORD {
    let side = |value: u16| i16::try_from(value.max(1)).unwrap_or(i16::MAX);
    COORD {
        X: side(cols),
        Y: side(rows),
    }
}

/// The output side of a [`Session`].
struct Output {
    /// Chunks from the reader thread; `None` once the session is being torn
    /// down.
    channel: Option<Receiver<Vec<u8>>>,
    /// The rest of a chunk the last read had no room for.
    carry: Vec<u8>,
}

/// A pipe pair, read end first.
fn pipe() -> Result<(HANDLE, HANDLE)> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    // SAFETY: both out-parameters are live, and no security attributes means
    // the handles are not inherited, which is what `spawn` needs to set up
    // explicitly for the console.
    unsafe { CreatePipe(&mut read, &mut write, None, 0) }.context("创建管道失败")?;
    Ok((read, write))
}

/// Starts `shell` attached to `console`.
fn spawn(shell: &str, console: HPCON) -> Result<(HANDLE, Job)> {
    // The job exists before the shell does, so the shell joins it before it
    // runs a single instruction and nothing it starts escapes.
    let job = Job::create()?;
    // `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` carries the console handle; the
    // attribute list has to be sized first, then filled.
    let mut size = 0usize;
    // SAFETY: a null list with a zero count only reports the size, which is
    // the documented way to size it.
    let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
    let mut storage = vec![0u8; size];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
    // SAFETY: `storage` is at least `size` bytes, as the call above reported.
    unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }
        .context("初始化进程属性失败")?;
    let attribute = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            // The attribute takes the `HPCON` *value*: the API stores the
            // pointer itself, so passing its address hands the console a
            // bogus handle and the child starts with no pseudoconsole at all.
            Some(console.0 as *const core::ffi::c_void),
            std::mem::size_of::<HPCON>(),
            None,
            None,
        )
    };
    if let Err(error) = attribute {
        // SAFETY: the list was initialized above and is not used again.
        unsafe { DeleteProcThreadAttributeList(list) };
        return Err(error).context("设置伪控制台属性失败");
    }

    // Quoted, so a shell under a path with spaces (a `ComSpec` in
    // `Program Files`) is not split into a program and arguments.
    let mut command: Vec<u16> = format!("\"{}\"", shell.trim_matches('"'))
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // The shell starts in the user's profile, as the POSIX one starts in
    // `$HOME`, rather than in whatever directory the host was launched from.
    let directory: Option<Vec<u16>> = std::env::var_os("USERPROFILE")
        .filter(|home| std::path::Path::new(home).is_dir())
        .map(|home| {
            use std::os::windows::ffi::OsStrExt;
            home.encode_wide().chain(std::iter::once(0)).collect()
        });
    let mut info = STARTUPINFOEXW::default();
    info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    info.lpAttributeList = list;
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: the command line is null-terminated and mutable, the startup
    // info is fully initialized, and the out-parameter is live.
    let created = unsafe {
        CreateProcessW(
            None,
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            None,
            directory
                .as_ref()
                .map_or(PCWSTR::null(), |directory| PCWSTR(directory.as_ptr())),
            &info.StartupInfo,
            &mut process,
        )
    };
    // SAFETY: the attribute list was initialized and is no longer needed once
    // the process exists.
    unsafe { DeleteProcThreadAttributeList(list) };
    created.context("启动 shell 失败")?;

    // The shell is still suspended: put it in the job, then let it run. A
    // shell that cannot be contained is not started at all.
    let started = job.assign(process.hProcess).and_then(|()| {
        // SAFETY: the thread handle came back from `CreateProcessW` and is
        // live; `u32::MAX` is the documented failure value.
        (unsafe { ResumeThread(process.hThread) } != u32::MAX)
            .then_some(())
            .context("启动 shell 失败")
    });
    // SAFETY: the thread handle is ours and not used again; on failure the
    // process handle is too.
    unsafe {
        let _ = CloseHandle(process.hThread);
        if started.is_err() {
            let _ = TerminateProcess(process.hProcess, 1);
            let _ = CloseHandle(process.hProcess);
        }
    }
    started.map(|()| (process.hProcess, job))
}

/// Reads the console's output into `out` until it closes.
fn read_output(handle: SendHandle, out: SyncSender<Vec<u8>>) {
    let handle = handle.0;
    let mut buffer = [0u8; 8192];
    loop {
        let mut read = 0u32;
        // SAFETY: the handle is live and the buffer is valid for writes of
        // its own length.
        let ok = unsafe {
            ReadFile(
                handle,
                Some(&mut buffer),
                Some(std::ptr::from_mut(&mut read)),
                None,
            )
        };
        if ok.is_err() || read == 0 {
            break;
        }
        // A closed receiver means the session is gone; stop reading.
        if out.send(buffer[..read as usize].to_vec()).is_err() {
            break;
        }
    }
    // SAFETY: the read end is ours and is not used again.
    unsafe {
        let _ = CloseHandle(handle);
    };
}

/// Writes queued input to the console until the channel closes.
fn write_input(handle: SendHandle, input: Receiver<Vec<u8>>, queued: Arc<AtomicUsize>) {
    let handle = handle.0;
    'chunks: while let Ok(data) = input.recv() {
        let mut rest = data.as_slice();
        while !rest.is_empty() {
            let mut written = 0u32;
            // SAFETY: the handle is live and the slice is valid for reads.
            let ok = unsafe {
                WriteFile(
                    handle,
                    Some(rest),
                    Some(std::ptr::from_mut(&mut written)),
                    None,
                )
            };
            if ok.is_err() || written == 0 {
                break 'chunks;
            }
            rest = &rest[written as usize..];
        }
        queued.fetch_sub(data.len(), Ordering::AcqRel);
    }
    // SAFETY: the write end is ours and is not used again.
    unsafe {
        let _ = CloseHandle(handle);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// `cmd.exe` is present on every Windows install.
    fn shell() -> &'static str {
        "cmd.exe"
    }

    /// Collects output until `needle` shows up or `timeout` passes.
    fn read_until(session: &mut Session, needle: &str, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        let mut buffer = [0u8; 4096];
        while Instant::now() < deadline {
            match session.read(&mut buffer) {
                Ok(Some(0)) => break,
                Ok(Some(read)) => seen.extend_from_slice(&buffer[..read]),
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => panic!("read: {error}"),
            }
            if String::from_utf8_lossy(&seen).contains(needle) {
                break;
            }
        }
        seen
    }

    #[test]
    fn a_shell_round_trips_through_the_pseudo_console() {
        let mut session = Session::open(shell(), 80, 24).expect("open");
        session.write(b"echo ROUND-TRIP-OK\r\n").expect("write");
        let seen = read_until(&mut session, "ROUND-TRIP-OK", Duration::from_secs(20));
        assert!(
            String::from_utf8_lossy(&seen).contains("ROUND-TRIP-OK"),
            "shell produced: {:?}",
            String::from_utf8_lossy(&seen)
        );
        // The controller's size must reach the console without error.
        session.resize(100, 40).expect("resize");
    }

    #[test]
    fn output_larger_than_the_read_buffer_is_not_lost() {
        let session = Session::open(shell(), 200, 24).expect("open");
        let marker = format!("LONG-{}-END", "x".repeat(120));
        session
            .write(format!("echo {marker}\r\n").as_bytes())
            .expect("write");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        // Far smaller than one console chunk, so every chunk is split.
        let mut buffer = [0u8; 16];
        while Instant::now() < deadline {
            match session.read(&mut buffer) {
                Ok(Some(0)) => break,
                Ok(Some(read)) => seen.extend_from_slice(&buffer[..read]),
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => panic!("read: {error}"),
            }
            // The echoed command line holds the marker too; the second copy
            // is the output.
            if String::from_utf8_lossy(&seen).matches(&marker).count() >= 2 {
                return;
            }
        }
        panic!("shell produced: {:?}", String::from_utf8_lossy(&seen));
    }

    #[test]
    fn an_exited_shell_is_reported() {
        let mut session = Session::open(shell(), 80, 24).expect("open");
        session.write(b"exit\r\n").expect("write");
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if session.exited() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("shell did not exit");
    }

    #[test]
    fn every_shell_kind_resolves_to_something() {
        for kind in ["powershell", "cmd", "bash", "", "nonsense"] {
            assert!(!resolve_shell(kind).is_empty(), "{kind:?}");
        }
        assert!(!default_shell().is_empty());
    }
}

impl Session {
    /// Ends the shell and everything it started.
    ///
    /// Closing the console alone only hangs the shell up and leaves whatever
    /// it spawned running; the job is what makes this mean the same thing as
    /// it does on Linux, where the shell's whole session is walked. `self`
    /// then drops, which closes the console and the handles. Dropping alone
    /// ends the job too, so this mainly states the intent at the call site.
    pub(crate) fn terminate(self) {
        self.job.terminate();
    }
}
