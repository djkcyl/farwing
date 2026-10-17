//! Serial notification I/O, coalesced outside the UI and media threads.
use super::{Action, Card, Mode};
use crate::platform::notifications::{Native, Toast, ToastAction};
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, mpsc};

pub(super) enum Event {
    Mode(Mode, u64),
    Error(Option<String>),
}
#[derive(Default)]
struct Latest {
    mode: Option<(Mode, u64)>,
    cards: Vec<Card>,
    closed: bool,
    folder: Option<std::path::PathBuf>,
}
pub(super) struct Worker {
    latest: Arc<Mutex<Latest>>,
    wake: mpsc::SyncSender<()>,
    pub events: mpsc::Receiver<Event>,
    thread: Option<std::thread::JoinHandle<()>>,
}
fn path() -> Result<std::path::PathBuf> {
    let base = crate::platform::paths::local_app_data().context("无法确定通知设置目录")?;
    Ok(base.join("OpenUUYC").join("notifications.json"))
}
fn load() -> Result<Mode> {
    match std::fs::read(path()?) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Mode::Gui),
        Err(e) => Err(e.into()),
    }
}
fn save(mode: Mode) -> Result<()> {
    let p = path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, serde_json::to_vec(&mode)?)?;
    if let Err(e) = std::fs::rename(&tmp, &p) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}
impl Worker {
    pub fn new(ctx: egui::Context) -> Self {
        let latest = Arc::new(Mutex::new(Latest::default()));
        let state = latest.clone();
        let (wake, rx) = mpsc::sync_channel(1);
        let (tx, events) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut mode = match load() {
                Ok(mode) => mode,
                Err(e) => {
                    let _ = tx.send(Event::Error(Some(format!("通知设置：{e}"))));
                    Mode::Gui
                }
            };
            {
                let mut s = super::lock(&state);
                if s.mode.is_none() {
                    s.mode = Some((mode, 0));
                }
            }
            let _ = tx.send(Event::Mode(mode, 0));
            ctx.request_repaint();
            let mut native: Option<Native> = None;
            let mut previous = Vec::<Toast>::new();
            let mut last_error = None;
            let mut initialization_error: Option<String> = None;
            let mut attempted_revision = 0;
            while rx.recv().is_ok() {
                let ((wanted, revision), cards, closed, folder) = {
                    let mut s = super::lock(&state);
                    (
                        s.mode.unwrap_or((mode, 0)),
                        s.cards.clone(),
                        s.closed,
                        s.folder.take(),
                    )
                };
                if closed {
                    break;
                }
                if attempted_revision != revision {
                    initialization_error = None;
                    attempted_revision = revision;
                }
                let result = (|| -> Result<()> {
                    if let Some(folder) = folder {
                        let text = folder.to_string_lossy();
                        let bytes = text.as_bytes();
                        anyhow::ensure!(
                            folder.is_absolute()
                                && bytes.len() > 2
                                && bytes[0].is_ascii_alphabetic()
                                && bytes[1] == b':'
                                && folder.is_dir(),
                            "文件所在目录已不可用"
                        );
                        crate::diagnostics::logging::open_folder(&folder)?;
                    }
                    if wanted != mode {
                        save(wanted)?;
                        mode = wanted;
                        let _ = tx.send(Event::Mode(mode, revision));
                    }
                    if !mode.windows() {
                        if let Some(n) = native.as_mut() {
                            n.clear()?;
                        }
                        native = None;
                        previous.clear();
                        return Ok(());
                    }
                    if cards.is_empty() && native.is_none() {
                        return Ok(());
                    }
                    if native.is_none() {
                        if let Some(error) = &initialization_error {
                            anyhow::bail!("{error}");
                        }
                        match Native::new() {
                            Ok(value) => native = Some(value),
                            Err(error) => {
                                let message = format!("{error:#}");
                                initialization_error = Some(message.clone());
                                anyhow::bail!("{message}");
                            }
                        }
                    }
                    let toasts = cards
                        .iter()
                        .map(|c| {
                            let buttons = c
                                .actions()
                                .into_iter()
                                .filter(|a| a.enabled)
                                .map(|a| ToastAction {
                                    label: a.label.into(),
                                    uri: Action::uri(&c.ticket, a.verb),
                                })
                                .collect();
                            Toast {
                                progress: c.transfer.as_ref().and_then(|t| t.progress),
                                key: c.ticket.clone(),
                                title: if c.connected && c.local_terminal {
                                    "对方接入了你本机的终端会话".into()
                                } else if c.connected && c.terminal {
                                    "终端连接已建立".into()
                                } else if c.connected {
                                    "远程连接已建立".into()
                                } else {
                                    c.title.clone()
                                },
                                body: format!(
                                    "{}\n{}",
                                    c.body,
                                    if c.local_terminal && c.connected {
                                        "对方能看到并操作你在本机打开的终端，打开控制中心可断开。"
                                    } else if c.terminal && c.connected {
                                        "对方可在本机执行命令，打开控制中心可断开。"
                                    } else if c.expires_at == 0 && c.transfer.is_none() {
                                        "打开控制中心查看当前连接状态。"
                                    } else {
                                        &c.detail
                                    }
                                ),
                                expires_at: c.expires_at,
                                buttons,
                            }
                        })
                        .collect::<Vec<_>>();
                    if toasts != previous {
                        native.as_mut().unwrap().synchronize(&toasts)?;
                        previous = toasts;
                    }
                    Ok(())
                })();
                let error = result.err().map(|e| format!("通知：{e:#}"));
                if error != last_error {
                    let _ = tx.send(Event::Error(error.clone()));
                    ctx.request_repaint();
                    last_error = error;
                }
            }
            if let Some(mut native) = native {
                let _ = native.clear();
            }
        });
        Self {
            latest,
            wake,
            events,
            thread: Some(thread),
        }
    }
    pub fn open_folder(&self, folder: std::path::PathBuf) {
        super::lock(&self.latest).folder = Some(folder);
        let _ = self.wake.try_send(());
    }
    pub fn set_mode(&self, mode: Mode, revision: u64) {
        super::lock(&self.latest).mode = Some((mode, revision));
        let _ = self.wake.try_send(());
    }
    pub fn update(&self, cards: &[Card]) {
        let mut s = super::lock(&self.latest);
        if s.cards != cards {
            s.cards = cards.to_vec();
            drop(s);
            let _ = self.wake.try_send(());
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        super::lock(&self.latest).closed = true;
        let _ = self.wake.try_send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
