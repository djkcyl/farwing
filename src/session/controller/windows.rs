//! Local GUI tasks; all HWNDs belong to the main window event loop.
use super::*;
use std::sync::Mutex;
use tokio::sync::watch;

#[derive(Clone, Default)]
pub(crate) struct ViewerInfo {
    pub target: Option<ViewerTarget>,
    pub connection: String,
    pub decoder: String,
    pub video_format: String,
    pub remote_encoder: String,
    pub remote_capture: String,
    pub playing: bool,
}
#[derive(Clone)]
pub(crate) struct ViewerTarget {
    pub device_id: String,
    pub alias: String,
}
pub(super) struct WindowContext {
    pub client: Arc<AuthenticatedClient>,
    pub cancel: CancellationToken,
    pub monitor: watch::Sender<Option<crate::diagnostics::performance::PerformanceMonitor>>,
    pub target: watch::Sender<Option<ViewerTarget>>,
    pub key: String,
    pub background: Option<crate::application::wallpaper::Source>,
    pub takeover: Option<super::takeover::Approval>,
}
pub(crate) struct ViewerHandle {
    controller_id: String,
    cancel: CancellationToken,
    info: Arc<Mutex<ViewerInfo>>,
    result: Arc<Mutex<Option<std::result::Result<ViewerEnd, String>>>>,
    key: String,
}

#[derive(Clone, Debug)]
pub(crate) enum ViewerEnd {
    Closed,
    RoomReleased,
    TakeoverRequired(Box<crate::account::api::DeviceInfo>, bool),
}

impl ViewerEnd {
    fn from_result(result: Result<()>, audio_only: bool) -> std::result::Result<Self, String> {
        match result {
            Ok(()) => Ok(Self::Closed),
            Err(error) if super::room_released(&error) => Ok(Self::RoomReleased),
            Err(error) if error.downcast_ref::<super::takeover::Required>().is_some() => {
                Ok(Self::TakeoverRequired(
                    Box::new(
                        error
                            .downcast_ref::<super::takeover::Required>()
                            .unwrap()
                            .0
                            .clone(),
                    ),
                    audio_only,
                ))
            }
            Err(error) => Err(format!("{error:#}")),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
impl ViewerHandle {
    pub fn request_audio_only(&self, enabled: bool) {
        if let Some(target) = lock(&self.info).target.as_ref()
            && let Some(session) =
                super::shared::get(&super::shared::key(&self.controller_id, &target.device_id))
        {
            session
                .peer
                .stream_control_handle()
                .request_audio_only(enabled);
        }
    }
    pub fn request_close(&self) {
        self.cancel.cancel();
    }
    pub fn info(&self) -> Option<ViewerInfo> {
        Some(lock(&self.info).clone())
    }
    pub fn result(&self) -> Option<std::result::Result<ViewerEnd, String>> {
        lock(&self.result).clone()
    }
    pub fn focus(&self) {
        let _ = crate::ui::window_manager::send(crate::ui::window_manager::Request::Focus(
            self.key.clone(),
        ));
    }
}
impl Drop for ViewerHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
fn jobs() -> &'static Mutex<Vec<(CancellationToken, CancellationToken)>> {
    static JOBS: std::sync::OnceLock<Mutex<Vec<(CancellationToken, CancellationToken)>>> =
        std::sync::OnceLock::new();
    JOBS.get_or_init(Mutex::default)
}
pub(crate) async fn shutdown() {
    let jobs = std::mem::take(&mut *lock(jobs()));
    for (cancel, _) in &jobs {
        cancel.cancel();
    }
    for (_, done) in jobs {
        done.cancelled().await;
    }
    crate::features::port_mapping::service::shutdown_all().await;
    crate::features::file_transfer::service::shutdown_all().await;
    crate::diagnostics::remote::service::shutdown_all().await;
    super::shared::shutdown_all().await;
}
pub(crate) fn start(
    client: Arc<AuthenticatedClient>,
    alias: String,
    id: Option<String>,
    assist: Option<crate::account::assist::AssistRequest>,
    options: ConnectionMediaOptions,
    background: Option<crate::application::wallpaper::Source>,
    takeover: Option<super::takeover::Approval>,
) -> ViewerHandle {
    let controller_id = client.device_id();
    let cancel = client.ended().child_token();
    let done = CancellationToken::new();
    {
        let mut jobs = lock(jobs());
        jobs.retain(|(_, done)| !done.is_cancelled());
        jobs.push((cancel.clone(), done.clone()));
    }
    let (monitor, monitor_rx) =
        watch::channel(None::<crate::diagnostics::performance::PerformanceMonitor>);
    let (target, target_rx) = watch::channel(id.as_ref().map(|id| ViewerTarget {
        device_id: id.clone(),
        alias: alias.clone(),
    }));
    let info = Arc::new(Mutex::new(ViewerInfo {
        target: target_rx.borrow().clone(),
        ..Default::default()
    }));
    let result = Arc::new(Mutex::new(None));
    let job_result = Arc::clone(&result);
    let job_info = Arc::clone(&info);
    let info_done = done.clone();
    let information = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        loop {
            tokio::select! {_=info_done.cancelled()=>break,_=tick.tick()=>{}}
            let mut info = ViewerInfo {
                target: target_rx.borrow().clone(),
                ..Default::default()
            };
            if let Some(m) = monitor_rx.borrow().as_ref() {
                let s = m.snapshot();
                info.connection = s.connection.clone();
                info.decoder = s.decoder.clone();
                info.video_format = s.video_format.clone();
                info.remote_encoder = s.remote_encoder.clone();
                info.remote_capture = s.remote_capture.clone();
                info.playing = s.total_rendered_frames > 0;
            }
            *lock(&job_info) = info;
        }
    });
    let key = format!("viewer:{}", uuid::Uuid::new_v4());
    let context = WindowContext {
        client,
        cancel: cancel.clone(),
        monitor,
        target,
        key: key.clone(),
        background,
        takeover,
    };
    tokio::spawn(async move {
        let _done = done.clone().drop_guard();
        let outcome =
            super::run_viewer_window(alias, options, id, assist, Some(context), false).await;
        *lock(&job_result) = Some(ViewerEnd::from_result(outcome, options.audio_only));
        done.cancel();
        let _ = information.await;
    });
    ViewerHandle {
        controller_id,
        cancel,
        info,
        result,
        key,
    }
}
