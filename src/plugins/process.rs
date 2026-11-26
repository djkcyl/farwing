use super::*;
#[cfg(windows)]
use std::os::windows::{io::AsRawHandle, process::CommandExt};
use std::{
    io::BufRead,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
#[cfg(windows)]
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0},
    System::{
        JobObjects::*,
        Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
    },
};

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
pub(crate) struct Sample {
    pub generation: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub at: Instant,
    pub pixels: Vec<u8>,
    pub motion: [i64; 2],
}
pub(crate) struct Display {
    pub reply: Option<Reply>,
    pub uploads: Vec<sdk::TextureUpload>,
    pub at: Instant,
    pub status: String,
}
#[derive(Clone)]
pub(crate) struct InputSample {
    pub generation: u64,
    pub sequence: u64,
    pub at: Instant,
    pub commands: Vec<sdk::InputCommand>,
    pub control_epoch: u64,
    pub motion: [i64; 2],
    pub dispatch_motion: [i64; 2],
    pub dispatch_corrections: [i64; 2],
}
pub(crate) struct Shared {
    pub enabled: AtomicBool,
    pub ready: AtomicBool,
    pub generation: AtomicU64,
    pub sample_fps: AtomicU64,
    pub signals: Mutex<std::collections::BTreeMap<u64, Arc<super::hotkeys::ControlGate>>>,
    pub physical: Mutex<Option<crate::features::remote_input::RemoteInput>>,
    pub view: Mutex<Option<sdk::Viewport>>,
    pub sample: Mutex<Option<Sample>>,
    pub display: Mutex<Display>,
    pub input: Mutex<std::collections::BTreeMap<u64, InputSample>>,
    context: egui::Context,
}
impl Shared {
    pub fn repaint(&self) {
        self.context.request_repaint();
    }
    pub fn new(context: egui::Context) -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            sample_fps: AtomicU64::new(10),
            signals: Mutex::new(std::collections::BTreeMap::new()),
            physical: Mutex::new(None),
            view: Mutex::new(None),
            sample: Mutex::new(None),
            display: Mutex::new(Display {
                reply: None,
                uploads: Vec::new(),
                at: Instant::now(),
                status: String::new(),
            }),
            input: Mutex::new(std::collections::BTreeMap::new()),
            context,
        })
    }
    pub fn submit(&self, mut sample: Sample) {
        sample.motion = lock(&self.physical).as_ref().map_or([0, 0], |p| p.motion());
        if self.ready.load(Ordering::Acquire)
            && let Ok(mut slot) = self.sample.try_lock()
        {
            *slot = Some(sample);
        }
    }
    pub fn fail(&self, error: String) {
        self.ready.store(false, Ordering::Release);
        self.enabled.store(false, Ordering::Release);
        let mut d = lock(&self.display);
        d.reply = None;
        d.uploads.clear();
        d.status = error;
        drop(d);
        self.context.request_repaint();
    }
}
pub(crate) struct Runtime {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    worker: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
}
impl Runtime {
    pub fn start(spec: graph::AnalysisSpec, shared: Arc<Shared>) -> Result<Self> {
        super::plan(&spec.instance.path, Some(&spec.instance.type_id))?;
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None::<Child>));
        let deadline = Arc::new(Mutex::new(Instant::now() + Duration::from_secs(120)));
        let worker_stop = stop.clone();
        let worker_child = child.clone();
        let worker_deadline = deadline.clone();
        let state = shared.clone();
        let worker=std::thread::Builder::new().name("Plugin IPC".into()).spawn(move||{
            let result=run(spec,&state,&worker_stop,&worker_child,&worker_deadline);
            if let Err(error)=result && !worker_stop.load(Ordering::Acquire){tracing::warn!(target:"openuuyc::plugins",error=%format!("{error:#}"),"plugin stopped");state.fail(format!("{error:#}"));}
            worker_stop.store(true,Ordering::Release);
            if let Some(mut process)=lock(&worker_child).take(){let _=process.kill();let _=process.wait();}
        })?;
        let timer_stop = stop.clone();
        let timer_child = child.clone();
        let watchdog = match std::thread::Builder::new()
            .name("Plugin watchdog".into())
            .spawn(move || {
                while !timer_stop.load(Ordering::Acquire) {
                    if !shared.enabled.load(Ordering::Acquire) || Instant::now() > *lock(&deadline)
                    {
                        if shared.enabled.load(Ordering::Acquire) {
                            shared.fail("插件响应超时，已停用".into());
                        }
                        timer_stop.store(true, Ordering::Release);
                        if let Some(child) = lock(&timer_child).as_mut() {
                            let _ = child.kill();
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }) {
            Ok(t) => t,
            Err(e) => {
                stop.store(true, Ordering::Release);
                if let Some(c) = lock(&child).as_mut() {
                    let _ = c.kill();
                }
                let _ = worker.join();
                return Err(e.into());
            }
        };
        Ok(Self {
            stop,
            child,
            worker: Some(worker),
            watchdog: Some(watchdog),
        })
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(child) = lock(&self.child).as_mut() {
            let _ = child.kill();
        }
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
        if let Some(t) = self.watchdog.take() {
            let _ = t.join();
        }
    }
}
fn run(
    spec: graph::AnalysisSpec,
    shared: &Shared,
    stop: &AtomicBool,
    child_slot: &Mutex<Option<Child>>,
    deadline: &Mutex<Instant>,
) -> Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("plugin-host")
        .arg(&spec.instance.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // CREATE_NO_WINDOW: a Windows child otherwise flashes a console.
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    crate::diagnostics::logging::configure_child(&mut command);
    let mut child = command.spawn().context("启动插件宿主")?;
    let job = match Job::attach(&child) {
        Ok(j) => j,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let mut input = child.stdin.take().context("plugin stdin")?;
    let mut output = child.stdout.take().context("plugin stdout")?;
    let stderr = child.stderr.take().context("plugin stderr")?;
    *lock(child_slot) = Some(child);
    let stderr_thread=std::thread::Builder::new().name("Plugin logs".into()).spawn(move||{let mut reader=std::io::BufReader::new(stderr);let mut line=String::new();while reader.by_ref().take(4096).read_line(&mut line).is_ok_and(|n|n>0){tracing::warn!(target:"openuuyc::plugins",message=%line.trim_end(),"plugin diagnostic");line.clear();}})?;
    let result = (|| {
        ensure!(!stop.load(Ordering::Acquire), "plugin cancelled");
        write_packet(&mut input, b"OpenUUYC plugin protocol 1")?;
        write_packet(&mut input, &serde_json::to_vec(&spec)?)?;
        ensure!(
            read_packet(&mut output, 64)? == b"ready",
            "plugin initialization handshake failed"
        );
        shared.ready.store(true, Ordering::Release);
        lock(&shared.display).status = "运行中".into();
        shared.context.request_repaint();
        let mut last_view = None;
        let mut last_sample = Instant::now();
        let mut sequence = 0;
        while !stop.load(Ordering::Acquire) && shared.enabled.load(Ordering::Acquire) {
            *lock(deadline) = Instant::now() + Duration::from_secs(3);
            let Some(view) = *lock(&shared.view) else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            let generation = shared.generation.load(Ordering::Acquire);
            let sample = lock(&shared.sample).take();
            if sample.is_none() && last_view == Some(view) {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            if let Some(s) = &sample {
                if s.generation != generation {
                    continue;
                }
                if s.at.elapsed() > Duration::from_millis(250) {
                    continue;
                }
                sequence = s.sequence;
                last_sample = s.at;
            }
            let frame_motion = sample.as_ref().map_or([0, 0], |s| s.motion);
            let feedback = lock(&shared.physical).as_ref().map(|p| p.observation());
            let request = Request {
                observation: sample.as_ref().map(|s| {
                    let (current_physical, submitted_motion, _, motion_pending, assist_pending) =
                        feedback.unwrap_or(([0, 0], [0, 0], [0, 0], false, false));
                    sdk::Observation {
                        generation: s.generation,
                        sequence: s.sequence,
                        physical_motion: s.motion,
                        current_physical,
                        submitted_motion,
                        motion_pending,
                        assist_pending,
                    }
                }),
                signals: lock(&shared.signals)
                    .iter()
                    .map(|(id, g)| {
                        let (active, epoch) = g.snapshot();
                        ControlSignal {
                            trigger: g.is_trigger(),
                            id: *id,
                            active,
                            epoch,
                        }
                    })
                    .collect(),
                generation,
                sequence,
                width: sample.as_ref().map_or(0, |s| s.width),
                height: sample.as_ref().map_or(0, |s| s.height),
                view,
            };
            let started = Instant::now();
            write_packet(&mut input, &serde_json::to_vec(&request)?)?;
            if let Some(sample) = sample {
                write_packet(&mut input, &sample.pixels)?;
            }
            let reply: Reply =
                serde_json::from_slice(&read_packet(&mut output, sdk::MAX_RENDER_BYTES)?)?;
            ensure!(
                reply.generation == request.generation
                    && reply.sequence == sequence
                    && reply.revision == view.revision,
                "plugin response identity mismatch"
            );
            validate(&reply.rendered)?;
            ensure!(
                reply.input.len() <= 4
                    && reply.input.iter().enumerate().all(|(i, batch)| {
                        spec.controls
                            .iter()
                            .any(|c| c.send_input && c.instance.id == batch.id)
                            && !reply.input[..i].iter().any(|old| old.id == batch.id)
                            && request.signals.iter().find(|s| s.id == batch.id).map_or(
                                batch.epoch == 0 && batch.commands.is_empty(),
                                |s| {
                                    s.epoch == batch.epoch
                                        && (s.active || batch.commands.is_empty())
                                },
                            )
                            && batch.commands.len() <= 8
                            && batch.commands.iter().all(|c| match c {
                                sdk::InputCommand::Relative { x, y } => {
                                    x.unsigned_abs() <= 512 && y.unsigned_abs() <= 512
                                }
                                sdk::InputCommand::Correction { x, y, weight, .. } => {
                                    x.unsigned_abs() <= 512
                                        && y.unsigned_abs() <= 512
                                        && weight
                                            .iter()
                                            .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
                                }
                                sdk::InputCommand::Click { hold_ms } => {
                                    (10..=100).contains(hold_ms)
                                }
                            })
                    }),
                "插件输入指令超限"
            );
            if request.width > 0 {
                tracing::trace!(target: "openuuyc::plugins", sequence, elapsed_ms = started.elapsed().as_secs_f64()*1000.0, "analysis frame completed");
            }
            let mut display = lock(&shared.display);
            let Reply {
                generation,
                sequence,
                revision,
                mut rendered,
                input,
            } = reply;
            if request.width > 0 {
                let mut samples = lock(&shared.input);
                for batch in &input {
                    let trigger = request
                        .signals
                        .iter()
                        .any(|s| s.id == batch.id && s.trigger);
                    if trigger && !batch.trigger {
                        continue;
                    }
                    samples.insert(
                        batch.id,
                        InputSample {
                            generation,
                            sequence,
                            at: last_sample,
                            commands: batch.commands.clone(),
                            control_epoch: batch.epoch,
                            motion: frame_motion,
                            dispatch_corrections: feedback.map_or([0; 2], |snapshot| snapshot.2),
                            dispatch_motion: request
                                .observation
                                .as_ref()
                                .map_or(frame_motion, |o| o.current_physical),
                        },
                    );
                }
            }
            display.uploads.append(&mut rendered.textures);
            ensure!(
                display.uploads.len() <= 64
                    && display
                        .uploads
                        .iter()
                        .map(|u| u.pixels.len())
                        .sum::<usize>()
                        <= 8 * 1024 * 1024,
                "插件纹理更新积压"
            );
            display.reply = Some(Reply {
                generation,
                sequence,
                revision,
                rendered,
                input,
            });
            display.at = last_sample;
            drop(display);
            shared.context.request_repaint();
            last_view = Some(view);
        }
        Ok(())
    })();
    shared.ready.store(false, Ordering::Release);
    drop(input);
    drop(output);
    drop(job);
    if let Some(child) = lock(child_slot).as_mut() {
        let _ = child.kill();
    }
    let _ = stderr_thread.join();
    result
}
fn validate(rendered: &sdk::Rendered) -> Result<()> {
    ensure!(
        rendered.layers.len() <= 64 && rendered.textures.len() <= 64,
        "render budget"
    );
    let mut vertices = 0;
    let mut indices = 0;
    let mut ids = BTreeSet::new();
    for layer in &rendered.layers {
        ensure!(
            valid_id(&layer.id)
                && ids.insert(&layer.id)
                && layer.name.len() <= 128
                && layer.meshes.len() <= 64,
            "invalid layer"
        );
        for mesh in &layer.meshes {
            vertices += mesh.vertices.len();
            indices += mesh.indices.len();
            ensure!(
                vertices <= 100000
                    && indices <= 300000
                    && mesh.indices.len() % 3 == 0
                    && mesh
                        .indices
                        .iter()
                        .all(|i| (*i as usize) < mesh.vertices.len()),
                "mesh budget"
            );
            ensure!(
                mesh.clip
                    .iter()
                    .all(|v| v.is_finite() && v.abs() <= 65536.0)
                    && mesh.vertices.iter().all(|v| v
                        .position
                        .iter()
                        .chain(v.uv.iter())
                        .all(|n| n.is_finite() && n.abs() <= 65536.0)),
                "invalid mesh coordinate"
            );
        }
    }
    let mut pixels = 0;
    for upload in &rendered.textures {
        let [w, h] = upload.size;
        pixels += upload.pixels.len();
        ensure!(
            w > 0
                && h > 0
                && w <= 4096
                && h <= 4096
                && w.checked_mul(h) == Some(upload.pixels.len())
                && pixels <= 4 * 1024 * 1024,
            "texture budget"
        );
    }
    Ok(())
}
#[cfg(windows)]
pub(super) struct Job(HANDLE);
#[cfg(windows)]
impl Job {
    pub(super) fn attach(child: &Child) -> Result<Self> {
        let job = Self(unsafe { CreateJobObjectW(None, None) }?);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                size_of_val(&info) as u32,
            )?;
            AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle()))?;
        }
        Ok(job)
    }
}
#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Linux has no job object; the host kills the plugin process itself so a
/// crashed or detached child cannot outlive the session that spawned it.
#[cfg(not(windows))]
pub(super) struct Job(u32);
#[cfg(not(windows))]
impl Job {
    pub(super) fn attach(child: &Child) -> Result<Self> {
        Ok(Self(child.id()))
    }
}
#[cfg(not(windows))]
impl Drop for Job {
    fn drop(&mut self) {
        // SIGKILL matches JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: no cleanup chance,
        // no chance to ignore it either.
        unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
    }
}
#[cfg(windows)]
pub(super) struct AnalysisSlot(HANDLE);
#[cfg(windows)]
impl AnalysisSlot {
    pub fn acquire() -> Result<Self> {
        for index in 1..=4 {
            let name: Vec<u16> = format!("Local\\OpenUUYC.AnalysisPlugin.{index}\0")
                .encode_utf16()
                .collect();
            let handle =
                unsafe { CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr())) }?;
            let status = unsafe { WaitForSingleObject(handle, 0) };
            if status == WAIT_OBJECT_0 || status == WAIT_ABANDONED {
                return Ok(Self(handle));
            }
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
        anyhow::bail!("同时运行的分析分支已达到4个")
    }
}
#[cfg(windows)]
impl Drop for AnalysisSlot {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// Four cross-process analysis branches, reserved with advisory file locks.
#[cfg(not(windows))]
pub(super) struct AnalysisSlot(std::fs::File);
#[cfg(not(windows))]
impl AnalysisSlot {
    pub fn acquire() -> Result<Self> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        for index in 1..=4 {
            let path = base.join(format!(
                "openuuyc-analysis-plugin-{}-{index}.lock",
                unsafe { libc::getuid() }
            ));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            match file.try_lock() {
                Ok(()) => return Ok(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        anyhow::bail!("同时运行的分析分支已达到4个")
    }
}
#[cfg(not(windows))]
impl Drop for AnalysisSlot {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
