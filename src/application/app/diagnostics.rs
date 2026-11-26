//! Local diagnostics use the same Windows fact collector as device registration.
use crate::media::LocalDisplayInfo;
use crate::media::decoder::diagnostics::{self as decoding, Event, Report, Status};

use std::sync::mpsc::{self, Receiver};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct LocalDiagnostics {
    pub rows: Vec<(String, String)>,
    pub graphics: Vec<String>,
    pub probe: Option<Report>,
    facts: Option<Receiver<Vec<(String, String)>>>,
    probing: Option<ProbeTask>,
}

struct ProbeTask {
    receiver: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for ProbeTask {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl LocalDiagnostics {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("local-hardware".into())
            .spawn(move || {
                let _ = tx.send(hardware());
            });
        Self {
            rows: Vec::new(),
            graphics: Vec::new(),
            probe: None,
            facts: Some(rx),
            probing: None,
        }
    }

    pub fn poll(&mut self) {
        if let Some(rx) = &self.facts {
            match rx.try_recv() {
                Ok(rows) => {
                    self.rows = rows;
                    self.facts = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.rows = vec![("硬件信息".into(), "读取失败".into())];
                    self.facts = None;
                }
                Err(_) => {}
            }
        }
        let mut finished = false;
        if let Some(task) = &self.probing {
            loop {
                let event = match task.receiver.try_recv() {
                    Ok(event) => event,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                };
                let report = self.probe.get_or_insert_with(Report::default);
                match event {
                    Event::Backend(backend) => report.backends.push(backend),
                    Event::Cell(backend, row, column, result) => {
                        report.backends[backend].rows[row].cells[column] = result;
                    }
                    Event::Finished(message) => {
                        report.message = message;
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            if let Some(report) = self.probe.as_mut() {
                if report.message.is_empty() {
                    report.message = "检查任务中断".into();
                }
                for cell in report
                    .backends
                    .iter_mut()
                    .flat_map(|b| &mut b.rows)
                    .flat_map(|r| &mut r.cells)
                    .filter(|c| c.status == Status::Pending)
                {
                    cell.status = Status::Unchecked;
                    cell.detail = report.message.clone();
                }
            }
            self.probing = None;
        }
    }

    pub fn busy(&self) -> bool {
        self.probing.is_some()
    }
    pub fn cancel_probe(&self) {
        if let Some(task) = &self.probing {
            task.cancel.store(true, Ordering::Release);
        }
    }
    pub fn probe(&mut self, display: LocalDisplayInfo) {
        if self.busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.probe = Some(Report::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("decoder-diagnostics".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decoding::run(display, &tx, worker_cancel.clone())
                }));
                let message = if worker_cancel.load(Ordering::Acquire) {
                    "检查已停止".into()
                } else {
                    match result {
                        Ok(Ok(())) => "检查完成".into(),
                        Ok(Err(error)) => format!("检查失败：{error:#}"),
                        Err(_) => "检查任务异常结束".into(),
                    }
                };
                let _ = tx.send(Event::Finished(message));
            });
        match thread {
            Ok(thread) => {
                self.probing = Some(ProbeTask {
                    receiver: rx,
                    cancel,
                    thread: Some(thread),
                })
            }
            Err(error) => self.probe.as_mut().unwrap().message = format!("无法启动检查：{error}"),
        }
    }
}

fn hardware() -> Vec<(String, String)> {
    match crate::platform::device_profile::Hardware::read() {
        Ok(hardware) => {
            let mut rows = vec![
                ("设备名称".into(), hardware.name),
                ("操作系统".into(), hardware.os),
                ("处理器".into(), hardware.cpu),
                ("主板".into(), hardware.base_board),
                ("物理内存".into(), format!("{} MiB", hardware.memory)),
            ];
            rows.extend(hardware.errors.into_iter().map(|e| ("读取提示".into(), e)));
            rows
        }
        Err(error) => vec![("硬件信息".into(), format!("读取失败：{error:#}"))],
    }
}
