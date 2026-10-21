//! Local diagnostics use the same Windows fact collector as device registration.
use crate::i18n::t;
use crate::media::LocalDisplayInfo;
use crate::media::decoder::diagnostics::{self as decoding, Event, Report, Status};

use std::sync::mpsc::{self, Receiver};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct LocalDiagnostics {
    pub rows: Vec<(Row, String)>,
    pub graphics: Vec<String>,
    pub probe: Option<Report>,
    facts: Option<Receiver<Vec<(Row, String)>>>,
    probing: Option<ProbeTask>,
}

/// What a hardware row shows; labelled in the current language when drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Row {
    DeviceName,
    OperatingSystem,
    Processor,
    Motherboard,
    Memory,
    ReadNote,
    Hardware,
}

impl Row {
    pub fn label(self) -> &'static str {
        let t = t();
        match self {
            Self::DeviceName => t.app.device_name,
            Self::OperatingSystem => t.settings.operating_system,
            Self::Processor => t.settings.processor,
            Self::Motherboard => t.settings.motherboard,
            Self::Memory => t.settings.memory,
            Self::ReadNote => t.app.read_note,
            Self::Hardware => t.app.hardware,
        }
    }
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
                    self.rows = vec![(Row::Hardware, t().app.read_failed.into())];
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
                    report.message = t().app.check_interrupted.into();
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
                let text = &t().app;
                let message = if worker_cancel.load(Ordering::Acquire) {
                    text.check_stopped.into()
                } else {
                    match result {
                        Ok(Ok(())) => text.check_complete.into(),
                        Ok(Err(error)) => (text.check_failed)(&format!("{error:#}")),
                        Err(_) => text.check_crashed.into(),
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
            Err(error) => {
                self.probe.as_mut().unwrap().message =
                    (t().app.cant_start_check)(&error.to_string())
            }
        }
    }
}

fn hardware() -> Vec<(Row, String)> {
    match crate::platform::windows::device_profile::Hardware::read() {
        Ok(hardware) => {
            let mut rows = vec![
                (Row::DeviceName, hardware.name),
                (Row::OperatingSystem, hardware.os),
                (Row::Processor, hardware.cpu),
                (Row::Motherboard, hardware.base_board),
                (Row::Memory, format!("{} MiB", hardware.memory)),
            ];
            rows.extend(hardware.errors.into_iter().map(|e| (Row::ReadNote, e)));
            rows
        }
        Err(error) => vec![(
            Row::Hardware,
            (t().app.read_failed_with)(&format!("{error:#}")),
        )],
    }
}
