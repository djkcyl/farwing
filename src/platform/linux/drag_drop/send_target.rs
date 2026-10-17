//! Session-owned send target: the small floating window files are dropped on
//! to queue them for the official controller, as on Windows. It appears while
//! a file drag is under way on the desktop -- an XDND source takes the
//! `XdndSelection` with the button held -- and while it holds a queue or a
//! result; the files come from the drop itself.
use anyhow::{Context as _, Result};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use x11rb::connection::Connection as _;
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{ConnectionExt as _, KeyButMask};

#[derive(Clone, Default, PartialEq)]
pub(crate) struct Model {
    pub epoch: u64,
    pub revision: u64,
    pub acknowledged: u64,
    pub enabled: bool,
    pub can_send: bool,
    pub status: Option<String>,
    pub items: Vec<Row>,
}
#[derive(Clone, PartialEq)]
pub(crate) struct Row {
    pub id: u64,
    pub name: String,
}
pub(crate) struct Action {
    pub epoch: u64,
    pub revision: u64,
    pub sequence: u64,
    pub kind: ActionKind,
}
pub(crate) enum ActionKind {
    Add(Vec<PathBuf>),
    Remove(u64),
    Clear,
    Send,
}

#[derive(Clone, Default, PartialEq)]
struct View {
    model: Model,
    issued: u64,
}
impl View {
    fn pending(&self) -> bool {
        self.issued > self.model.acknowledged
    }
    fn submit(&mut self, send: &mpsc::SyncSender<Action>, kind: ActionKind) -> bool {
        if !self.model.enabled || self.pending() {
            return false;
        }
        let sequence = self.issued.wrapping_add(1);
        if send
            .try_send(Action {
                epoch: self.model.epoch,
                revision: self.model.revision,
                sequence,
                kind,
            })
            .is_err()
        {
            return false;
        }
        self.issued = sequence;
        true
    }
}

struct Shared {
    view: Mutex<View>,
    send: mpsc::SyncSender<Action>,
    stop: AtomicBool,
    /// A file drag is under way on the desktop.
    dragging: AtomicBool,
    /// The monitor the pointer was on when it began: left, top and width in
    /// physical pixels.
    monitor: Mutex<Option<(i32, i32, i32)>>,
    context: Mutex<Option<egui::Context>>,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
impl Shared {
    fn repaint(&self) {
        if let Some(context) = lock(&self.context).as_ref() {
            context.request_repaint();
        }
    }
}

pub(crate) struct SendTarget {
    shared: Arc<Shared>,
    watcher: Option<std::thread::JoinHandle<()>>,
}
impl SendTarget {
    pub fn start(send: mpsc::SyncSender<Action>) -> Result<Self> {
        let shared = Arc::new(Shared {
            view: Mutex::new(View::default()),
            send,
            stop: AtomicBool::new(false),
            dragging: AtomicBool::new(false),
            monitor: Mutex::new(None),
            context: Mutex::new(None),
        });
        let watching = shared.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let watcher = std::thread::Builder::new()
            .name("file send target".into())
            .spawn(move || {
                if let Err(error) = watch(&watching, &ready) {
                    let message = format!("{error:#}");
                    let _ = ready.try_send(Err(message.clone()));
                    tracing::warn!(%message, "file send target stopped");
                }
            })?;
        let mut target = Self {
            shared,
            watcher: Some(watcher),
        };
        match started.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            error => {
                target.shared.stop.store(true, Ordering::Release);
                target.watcher.take();
                anyhow::bail!("文件发送浮窗初始化失败: {error:?}")
            }
        }
        let window = target.shared.clone();
        use crate::ui::theme;
        crate::ui::window_manager::send(crate::ui::window_manager::Request::Open {
            key: format!("file-send-target:{}", uuid::Uuid::new_v4().simple()),
            config: crate::ui::WindowConfig {
                viewport: egui::ViewportBuilder::default()
                    .with_title("OpenUUYC · 发送到主控")
                    .with_inner_size([theme::FILE_SEND_WIDTH, theme::FILE_SEND_HEIGHT])
                    .with_resizable(false)
                    .with_visible(false)
                    .with_active(false)
                    .with_taskbar(false)
                    .with_window_type(egui::X11WindowType::Utility)
                    .with_always_on_top(),
                centered: false,
                notification: false,
                floating: true,
            },
            factory: Box::new(move |context, _| {
                theme::configure(context);
                *lock(&window.context) = Some(context.clone());
                Box::new(Window {
                    shared: window,
                    visible: false,
                    height: 0.,
                    moved: false,
                })
            }),
        })?;
        tracing::info!("official file send drag listener started");
        Ok(target)
    }
    pub fn update(&self, model: Model) {
        let changed = {
            let mut view = lock(&self.shared.view);
            if view.model.epoch != model.epoch || !model.enabled {
                view.issued = model.acknowledged;
            } else {
                view.issued = view.issued.max(model.acknowledged);
            }
            let changed = view.model != model;
            view.model = model;
            changed
        };
        if changed {
            self.shared.repaint();
        }
    }
}
impl Drop for SendTarget {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.repaint();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// Follow file drags on the desktop: a drag source takes `XdndSelection`
/// while the button is down. Drags this process runs itself are not offers.
fn watch(shared: &Shared, ready: &mpsc::SyncSender<Result<(), String>>) -> Result<()> {
    if std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland") {
        anyhow::bail!("Wayland 桌面暂不支持文件发送浮窗");
    }
    let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
    let root = connection.setup().roots[screen].root;
    connection.xfixes_query_version(5, 0)?.reply()?;
    let selection = connection
        .intern_atom(false, b"XdndSelection")?
        .reply()?
        .atom;
    connection.xfixes_select_selection_input(
        root,
        selection,
        SelectionEventMask::SET_SELECTION_OWNER,
    )?;
    connection.flush()?;
    let _ = ready.send(Ok(()));
    let mut armed = false;
    // Until a drag says otherwise, the window goes to the pointer's monitor.
    if let Ok(pointer) = connection.query_pointer(root)?.reply() {
        *lock(&shared.monitor) = monitor_at(pointer.root_x.into(), pointer.root_y.into());
    }
    while !shared.stop.load(Ordering::Acquire) {
        while let Some(event) = connection.poll_for_event()? {
            if let XEvent::XfixesSelectionNotify(event) = event
                && event.selection == selection
                && event.owner != x11rb::NONE
                && !super::own_drag()
            {
                armed = true;
            }
        }
        let pointer = connection.query_pointer(root)?.reply()?;
        let held = pointer.mask.contains(KeyButMask::BUTTON1);
        if !held || pointer.mask.contains(KeyButMask::BUTTON3) {
            armed = false;
        }
        let dragging = held && armed;
        if shared.dragging.swap(dragging, Ordering::AcqRel) != dragging {
            if dragging {
                *lock(&shared.monitor) = monitor_at(pointer.root_x.into(), pointer.root_y.into());
            }
            shared.repaint();
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    Ok(())
}

/// Left, top and width of the monitor holding a root point.
fn monitor_at(x: i32, y: i32) -> Option<(i32, i32, i32)> {
    let screens = crate::platform::capture::screens().ok()?;
    screens
        .iter()
        .find(|s| {
            x >= s.left && y >= s.top && x < s.left + s.width as i32 && y < s.top + s.height as i32
        })
        .or_else(|| screens.iter().find(|s| s.primary))
        .or_else(|| screens.first())
        .map(|s| (s.left, s.top, s.width as i32))
}

struct Window {
    shared: Arc<Shared>,
    visible: bool,
    height: f32,
    /// The user dragged the window; keep it where they put it.
    moved: bool,
}
impl crate::ui::App for Window {
    fn uses_tray(&self) -> bool {
        false
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        use crate::ui::theme;
        let context = ui.ctx().clone();
        if self.shared.stop.load(Ordering::Acquire) {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        context.request_repaint_after(Duration::from_millis(250));
        let hovered = context.input(|i| !i.raw.hovered_files.is_empty());
        let dropped: Vec<PathBuf> = context.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        let mut view = lock(&self.shared.view);
        if !dropped.is_empty() {
            view.submit(&self.shared.send, ActionKind::Add(dropped));
        }
        let snapshot = view.clone();
        drop(view);
        let model = &snapshot.model;
        let pending = snapshot.pending();
        let show = model.enabled
            && (hovered
                || self.shared.dragging.load(Ordering::Acquire)
                || pending
                || !model.items.is_empty()
                || model.status.is_some());
        let rows = model.items.len().min(theme::FILE_SEND_VISIBLE_ROWS);
        let height = theme::FILE_SEND_HEIGHT
            + if model.items.is_empty() {
                0.
            } else {
                rows as f32 * theme::FILE_SEND_ROW_HEIGHT + 40.
            };
        if show && (!self.visible || self.height != height) {
            context.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                theme::FILE_SEND_WIDTH,
                height,
            )));
            self.height = height;
        }
        if show && !self.visible {
            if !self.moved
                && let Some((left, top, width)) = *lock(&self.shared.monitor)
            {
                let scale = context.pixels_per_point().max(0.1);
                let x = left as f32 / scale + (width as f32 / scale - theme::FILE_SEND_WIDTH) / 2.;
                let y = top as f32 / scale + 24.;
                context.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
            }
            context.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::AlwaysOnTop,
            ));
            context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        } else if !show && self.visible {
            context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        self.visible = show;
        let rect = ui.max_rect();
        ui.painter().rect(
            rect,
            0.,
            if hovered {
                theme::SELECTED
            } else {
                theme::SURFACE
            },
            egui::Stroke::new(1., if hovered { theme::ACCENT } else { theme::LINE }),
            egui::StrokeKind::Inside,
        );
        let title = if hovered {
            "松开加入待发送队列".to_owned()
        } else if pending {
            "正在处理…".to_owned()
        } else if let Some(status) = &model.status {
            status.clone()
        } else if !model.items.is_empty() {
            format!("待发送 · {} 项", model.items.len())
        } else {
            "拖入文件或文件夹".to_owned()
        };
        let caption = if model.items.len() > rows {
            "可滚动查看 · 点击发送后开始传输"
        } else {
            "可继续拖入 · 点击发送后开始传输"
        };
        // Only the header moves the window; rows and buttons keep their input.
        let header = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 64.));
        let response = ui.interact(header, ui.id().with("header"), egui::Sense::drag());
        if response.drag_started() {
            self.moved = true;
            context.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let painter = ui.painter();
        let font = egui::FontId::proportional(theme::BODY);
        painter.text(
            egui::pos2(rect.center().x, rect.min.y + 24.),
            egui::Align2::CENTER_CENTER,
            title,
            font.clone(),
            theme::TEXT,
        );
        painter.text(
            egui::pos2(rect.center().x, rect.min.y + 52.),
            egui::Align2::CENTER_CENTER,
            caption,
            egui::FontId::proportional(theme::BODY - 1.),
            theme::MUTED,
        );
        if model.items.is_empty() {
            return;
        }
        let enabled = model.enabled && !pending;
        let list = egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 12., rect.min.y + 70.),
            egui::pos2(
                rect.max.x - 12.,
                rect.min.y + 70. + rows as f32 * theme::FILE_SEND_ROW_HEIGHT,
            ),
        );
        let mut action = None;
        ui.scope_builder(egui::UiBuilder::new().max_rect(list), |ui| {
            egui::ScrollArea::vertical()
                .max_height(list.height())
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.;
                    for item in &model.items {
                        let (row, _) = ui.allocate_exact_size(
                            egui::vec2(list.width(), theme::FILE_SEND_ROW_HEIGHT),
                            egui::Sense::hover(),
                        );
                        let remove = egui::Rect::from_min_max(
                            egui::pos2(row.max.x - 24., row.min.y),
                            row.max,
                        );
                        if ui
                            .put(remove, egui::Button::new("×").frame(false))
                            .clicked()
                            && enabled
                        {
                            action = Some(ActionKind::Remove(item.id));
                        }
                        let name = egui::Rect::from_min_max(
                            row.min,
                            egui::pos2(row.max.x - 30., row.max.y),
                        );
                        ui.scope_builder(
                            egui::UiBuilder::new().max_rect(name).layout(
                                egui::Layout::left_to_right(egui::Align::Center)
                                    .with_cross_justify(true),
                            ),
                            |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&item.name).color(theme::TEXT),
                                    )
                                    .truncate()
                                    .halign(egui::Align::Min),
                                );
                            },
                        );
                    }
                });
        });
        let top = rect.min.y + height - 40.;
        let half = (theme::FILE_SEND_WIDTH - 32.) / 2.;
        let clear =
            egui::Rect::from_min_size(egui::pos2(rect.min.x + 12., top), egui::vec2(half, 28.));
        let send = egui::Rect::from_min_size(
            egui::pos2(rect.min.x + 20. + half, top),
            egui::vec2(half, 28.),
        );
        if ui
            .put(clear, egui::Button::new("清空队列"))
            .on_disabled_hover_text("正在处理…")
            .clicked()
            && enabled
        {
            action = Some(ActionKind::Clear);
        }
        let label = format!("发送 ({})", model.items.len());
        if ui
            .add_enabled_ui(model.can_send && enabled, |ui| {
                ui.put(send, crate::ui::controls::primary(&label))
            })
            .inner
            .clicked()
        {
            action = Some(ActionKind::Send);
        }
        if let Some(kind) = action {
            lock(&self.shared.view).submit(&self.shared.send, kind);
            context.request_repaint();
        }
    }
}
