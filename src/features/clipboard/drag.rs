//! Official clipboard-action adapter. The wire has no offer identifier: one
//! unclaimed explicit selection per connection is bound by the next descriptor
//! request, then the existing task table owns it. Ordinary clipboard stays separate.
use super::*;
use crate::protocol::drag_drop::Point;
use std::{path::PathBuf, time::Instant};
pub(crate) enum SubmissionState {
    Preparing,
    Waiting,
    HandedOff,
    Complete { total: u32, saved: u32 },
    Failed(String),
}

struct PointerLease {
    session: Arc<Inner>,
    active: AtomicBool,
}
impl PointerLease {
    fn new(session: Arc<Inner>) -> Arc<Self> {
        session.drop_pointer.fetch_add(1, Ordering::AcqRel);
        Arc::new(Self {
            session,
            active: AtomicBool::new(true),
        })
    }
    fn release(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            self.session.drop_pointer.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
impl Drop for PointerLease {
    fn drop(&mut self) {
        self.release();
    }
}
pub(crate) struct Submission {
    pub(super) id: u64,
    pub(super) session: Weak<Inner>,
    pub(super) epoch: u64,
    pub(super) created: Instant,
    state: Mutex<SubmissionState>,
    auto_save: bool,
    pub(super) published: AtomicBool,
}
impl Submission {
    pub fn state(&self) -> SubmissionState {
        use SubmissionState::*;
        let state = lock(&self.state);
        match &*state {
            Complete { total, saved } => Complete {
                total: *total,
                saved: *saved,
            },
            HandedOff
                if self.auto_save
                    && !self
                        .session
                        .upgrade()
                        .is_some_and(|s| s.valid(self.epoch) && s.file_allowed()) =>
            {
                Failed("连接或文件权限已结束".into())
            }
            HandedOff => HandedOff,
            Failed(e) => Failed(e.clone()),
            _ if !self.valid() => {
                self.release();
                Failed("文件拖放已结束或等待超时".into())
            }
            Preparing => Preparing,
            Waiting => Waiting,
        }
    }
    pub fn cancel(&self) {
        self.fail("拖放已取消".into());
    }
    pub(super) fn valid(&self) -> bool {
        self.created.elapsed() < Duration::from_secs(30)
            && self.session.upgrade().is_some_and(|s| {
                s.valid(self.epoch)
                    && s.file_allowed()
                    && s.explicit_drop.load(Ordering::Acquire) == self.id
            })
    }
    pub(super) fn handed_off(&self) {
        *lock(&self.state) = SubmissionState::HandedOff;
        if self.auto_save {
            tracing::info!("official file send accepted by controller");
        }
        if !self.auto_save {
            self.release();
        }
    }
    pub(super) fn fail(&self, message: String) {
        *lock(&self.state) = SubmissionState::Failed(message);
        self.release();
        // Before publication a preparation failure has no remote side effect.
        // Release the slot so an explicitly edited draft can be sent again.
        if self.auto_save
            && !self.published.load(Ordering::Acquire)
            && let Some(session) = self.session.upgrade()
        {
            let mut pending = lock(&session.auto_save);
            if pending.as_ref().is_some_and(|ticket| ticket.id == self.id) {
                pending.take();
            }
        }
    }
    fn release(&self) {
        if let Some(s) = self.session.upgrade() {
            let _ =
                s.explicit_drop
                    .compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}
impl Drop for Submission {
    fn drop(&mut self) {
        self.release();
    }
}
impl Clipboard {
    pub(crate) fn drop_available(&self) -> bool {
        self.0.file_offers.is_none()
            && lock(&self.0.auto_save).is_none()
            && self.0.valid(self.epoch())
            && self.0.file_allowed()
            && self.0.explicit_drop.load(Ordering::Acquire) == 0
    }
    pub(crate) fn drop_files(&self, paths: Vec<PathBuf>, point: Point) -> Result<Arc<Submission>> {
        ensure!(point.valid(), "拖放落点无效");
        self.submit_files(
            paths,
            ClipboardFormatListRequestKind::OleDrop(DragDropOleDrop {
                screen_id: point.screen,
                target_x: point.x,
                target_y: point.y,
            }),
        )
    }
    pub(crate) fn send_files(&self, paths: Vec<PathBuf>) -> Result<Arc<Submission>> {
        ensure!(
            self.0.host_role.load(Ordering::Acquire),
            "只有被控端可发送到主控接收目录"
        );
        self.submit_files(
            paths,
            ClipboardFormatListRequestKind::AutoSave(DragDropAutoSave {
                dest_path: String::new(),
            }),
        )
    }
    fn submit_files(
        &self,
        paths: Vec<PathBuf>,
        action: ClipboardFormatListRequestKind,
    ) -> Result<Arc<Submission>> {
        ensure!(
            self.drop_available(),
            "需要开启文件剪贴板与远端控制，且上次拖放已被接收"
        );
        let id = (uuid::Uuid::new_v4().as_u128() as u64).max(1);
        ensure!(
            self.0
                .explicit_drop
                .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "上次拖放尚未被接收"
        );
        let ticket = Arc::new(Submission {
            id,
            session: Arc::downgrade(&self.0),
            epoch: self.epoch(),
            created: Instant::now(),
            state: Mutex::new(SubmissionState::Preparing),
            auto_save: matches!(&action, ClipboardFormatListRequestKind::AutoSave(_)),
            published: AtomicBool::new(false),
        });
        if ticket.auto_save {
            *lock(&self.0.auto_save) = Some(ticket.clone());
        }
        let working = ticket.clone();
        std::thread::Builder::new()
            .name("file drop preparation".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let prepared = native::prepare_files(paths, || working.valid())?;
                    ensure!(working.valid(), "拖放已取消");
                    *lock(&working.state) = SubmissionState::Waiting;
                    native::post(native::Command::DropFiles(
                        working.clone(),
                        prepared,
                        action,
                    ))
                })();
                if let Err(error) = result {
                    working.fail(error.to_string());
                }
            })?;
        Ok(ticket)
    }
    pub(super) fn auto_save_complete(&self, report: &DragDropAutoSaveComplete) {
        let mut pending = lock(&self.0.auto_save);
        if let Some(ticket) = pending.as_ref()
            && ticket.epoch == self.epoch()
            && matches!(*lock(&ticket.state), SubmissionState::HandedOff)
            && report.success_count <= report.total_count
        {
            tracing::info!(
                total = report.total_count,
                saved = report.success_count,
                "official file send saved by controller"
            );
            *lock(&ticket.state) = SubmissionState::Complete {
                total: report.total_count,
                saved: report.success_count,
            };
            ticket.release();
            pending.take();
        }
    }
}

/// The official protocol provides a final drop action, not a live drag stream.
/// Execute it in the interactive user, with the same scoped OLE file provider.
pub(super) fn receive(
    offer: native::FileOffer,
    action: ClipboardFormatListRequestKind,
) -> Result<()> {
    let session = offer.0.session.clone();
    ensure!(
        session
            .incoming_drops
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < 4)
                .then_some(n + 1))
            .is_ok(),
        "正在接收的拖放过多"
    );
    struct Active(Arc<Inner>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.incoming_drops.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let active = Active(session);
    // Claim before requesting descriptors: the parent can stop ordinary motion
    // while metadata is in flight, before the final-point OLE source starts.
    let pointer = matches!(&action, ClipboardFormatListRequestKind::OleDrop(_))
        .then(|| PointerLease::new(offer.0.session.clone()));
    std::thread::Builder::new().name("official file drop".into()).spawn(move || {
        let _active=active;
        let result=(||->Result<()> {
            ensure!(crate::features::host::clipboard::agent::desktop_available(),"用户桌面不可用");
            offer.prepare()?;
            match action {
                ClipboardFormatListRequestKind::AutoSave(action)=>{
                    let manifest=offer.manifest()?;
                    let total=manifest.len() as u32;
                    let run=(||->Result<crate::features::file_transfer::import::Report> {
                        let root=crate::features::file_transfer::import::destination(&action.dest_path)?;
                        let items=manifest.into_iter().map(|f|crate::features::file_transfer::import::Item {
                            directory:f.file_attributes&0x10!=0,
                            info:crate::features::file_transfer::protocol::FileInfo {
                                rel_path:f.file_name,size:f.file_size,
                                modified_time:wire_seconds(f.last_write_time),
                            },
                        }).collect();
                        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                        runtime.block_on(crate::features::file_transfer::import::receive(root,items,
                            ||offer.valid() && crate::features::host::clipboard::agent::desktop_available(),
                            |index,offset,length|offer.read(index,offset,length)))
                    })();
                    let (count,saved,first)=match &run {Ok(report)=>(report.total,report.saved,report.first.clone()),Err(_)=>(total,0,String::new())};
                    offer.0.session.emit(offer.0.epoch,request(offer.0.session.next(),ClipboardRequestKind::AutoSaveComplete(
                        DragDropAutoSaveComplete{total_count:count,success_count:saved,first_file_name:first})))?;
                    run?;
                }
                ClipboardFormatListRequestKind::OleDrop(action)=>{
                    let point=Point{screen:action.screen_id,x:action.target_x,y:action.target_y};
                    ensure!(point.valid(),"拖放落点无效");
                    let screens=crate::platform::capture::screens()?;
                    let screen=screens.iter().find(|s|s.id==point.screen && s.width>0 && s.height>0).ok_or_else(||anyhow!("拖放显示器已断开"))?;
                    let position=crate::platform::drag_drop::Position {
                        x:i32::try_from(i64::from(screen.left)+(point.x*f64::from(screen.width)).round().clamp(0.,f64::from(screen.width-1)) as i64)?,
                        y:i32::try_from(i64::from(screen.top)+(point.y*f64::from(screen.height)).round().clamp(0.,f64::from(screen.height-1)) as i64)?,
                    };
                    let (tx,rx)=std::sync::mpsc::channel();
                    let permitted=offer.clone();let file=offer.clone();
                    offer.dragging(true);
                    let drag=crate::platform::drag_drop::Session::start(position,
                        Arc::new(move ||permitted.valid() && crate::features::host::clipboard::agent::desktop_available()),
                        move ||file.object(),Arc::new(move |event|{
                            if matches!(event,crate::platform::drag_drop::Event::Released)
                                && let Some(pointer)=&pointer {pointer.release();}
                            let _=tx.send(event);
                        }))?;
                    let deadline=Instant::now()+Duration::from_secs(10);let mut committed=false;
                    loop {
                        ensure!(offer.valid() && Instant::now()<deadline,"拖放已取消或目标未响应");
                        match rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(crate::platform::drag_drop::Event::Feedback(effect)) if !committed=>{
                                ensure!(effect & 1 != 0,"落点不能接收文件");
                                offer.dragging(false);drag.commit(position)?;committed=true;
                            }
                            Ok(crate::platform::drag_drop::Event::Finished{accepted,error,..})=>{
                                ensure!(accepted,"{}",error.unwrap_or_else(||"目标未接收文件".into()));break;
                            }
                            Ok(_)|Err(std::sync::mpsc::RecvTimeoutError::Timeout)=>{},
                            Err(_)=>anyhow::bail!("拖放执行已结束"),
                        }
                    }
                    loop {
                        ensure!(offer.valid(),"文件接收已取消");
                        if let Some(result)=offer.status().completed {result.map_err(anyhow::Error::msg)?;break;}
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            }
            Ok(())
        })();
        if let Err(error)=result {tracing::warn!(%error,"official clipboard drop failed");offer.0.session.fail(error.to_string());}
        offer.cancel();
    })?;
    Ok(())
}
