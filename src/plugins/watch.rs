use super::{graph::*, *};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

pub(crate) struct Update {
    pub revision: String,
    pub result: std::result::Result<Compiled, String>,
}
pub(crate) struct Watcher {
    receiver: mpsc::Receiver<Update>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Watcher {
    pub fn start(id: String, ctx: egui::Context) -> Result<Self> {
        let source = path(&id, true)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        let worker = std::thread::Builder::new()
            .name("Graph updates".into())
            .spawn(move || {
                let mut previous = None;
                let mut missing_reported = false;
                while !cancelled.load(Ordering::Acquire) {
                    if let Ok(metadata) = std::fs::metadata(&source) {
                        let stamp = (metadata.modified().ok(), metadata.len());
                        if previous != Some(stamp) {
                            let result = (|| -> Result<(String, Compiled)> {
                                ensure!(metadata.len() <= 1024 * 1024, "节点图文件过大");
                                let publication: Publication =
                                    serde_json::from_slice(&std::fs::read(&source)?)?;
                                ensure!(publication.document.graph_id == id, "节点图身份不匹配");
                                let catalog = Catalog::load()?;
                                let plan =
                                    compile(&publication.document, &catalog).map_err(|d| {
                                        anyhow::anyhow!(
                                            d.into_iter()
                                                .map(|d| d.message)
                                                .collect::<Vec<_>>()
                                                .join("\n")
                                        )
                                    })?;
                                Ok((publication.revision, plan))
                            })();
                            let update = match result {
                                Ok((revision, plan)) => Update {
                                    revision,
                                    result: Ok(plan),
                                },
                                Err(error) => Update {
                                    revision: String::new(),
                                    result: Err(format!("{error:#}")),
                                },
                            };
                            match sender.try_send(update) {
                                Ok(()) => {
                                    previous = Some(stamp);
                                    ctx.request_repaint();
                                }
                                Err(mpsc::TrySendError::Full(_)) => {}
                                Err(_) => break,
                            }
                        }
                    } else if !missing_reported
                        && sender
                            .try_send(Update {
                                revision: String::new(),
                                result: Err("此图尚未发布，请在编辑器中点击应用".into()),
                            })
                            .is_ok()
                    {
                        missing_reported = true;
                        ctx.request_repaint();
                    }
                    std::thread::park_timeout(Duration::from_millis(250));
                }
            })?;
        Ok(Self {
            receiver,
            stop,
            worker: Some(worker),
        })
    }
    pub fn poll(&self) -> Option<Update> {
        self.receiver.try_recv().ok()
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}
