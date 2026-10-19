//! One viewer drop target, bound to its current video geometry and peer.
use super::windows_mouse::Target;
use crate::{
    features::{
        drag_drop::{Stage, Ticket, controller::Controller},
        stream_control::StreamControlHandle,
    },
    platform::windows::drag_drop::target::{Handler, Registration},
    protocol::drag_drop::{COPY, Point},
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};
use windows::Win32::Foundation::{POINT, POINTL};

pub(super) struct WindowDrag {
    _registration: Registration,
    state: Rc<RefCell<State>>,
}
struct State {
    owner: u64,
    controller: Controller,
    control: StreamControlHandle,
    geometry: Arc<Mutex<Option<Target>>>,
    paths: Vec<std::path::PathBuf>,
    preview: Option<Arc<Ticket>>,
    returning: Option<Arc<Ticket>>,
    transfers: Vec<Arc<Ticket>>,
    error: Option<String>,
}
impl WindowDrag {
    pub fn new(
        window: &winit::window::Window,
        control: &StreamControlHandle,
    ) -> anyhow::Result<Self> {
        let state = Rc::new(RefCell::new(State {
            owner: crate::platform::graphics::window_hwnd(window)?.0 as u64,
            controller: control.drag_drop().clone(),
            control: control.clone(),
            geometry: Arc::new(Mutex::new(None)),
            paths: Vec::new(),
            preview: None,
            returning: None,
            transfers: Vec::new(),
            error: None,
        }));
        let registration = Registration::new(
            crate::platform::graphics::window_hwnd(window)?,
            state.clone(),
        )?;
        Ok(Self {
            _registration: registration,
            state,
        })
    }
    pub fn geometry(&self, geometry: Option<Target>) {
        let Ok(mut state) = self.state.try_borrow_mut() else {
            return;
        };
        if geometry.is_none() {
            state.cancel_preview();
        }
        *state.geometry.lock().unwrap_or_else(|e| e.into_inner()) = geometry;
    }
    pub fn show(&self, ctx: &egui::Context) {
        let Ok(mut state) = self.state.try_borrow_mut() else {
            return;
        };
        let reverse = state.controller.take_reverse(state.owner);
        state.transfers.extend(reverse);
        state.transfers.retain(|ticket| {
            let snapshot = ticket.snapshot();
            !matches!(snapshot.stage, Stage::Complete | Stage::Cancelled)
        });
        if state.preview.is_none() && state.transfers.is_empty() && state.error.is_none() {
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        egui::Area::new(egui::Id::new("native-file-drag"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0., -18.])
            .order(egui::Order::Foreground)
            // A passive drag hint must not become a forbidden drop target and
            // repeatedly cancel/restart the preview underneath itself.
            .interactable(state.paths.is_empty())
            .movable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(360.);
                    if let Some(ticket) = &state.preview {
                        let snapshot = ticket.snapshot();
                        ui.label(match snapshot.stage {
                            Stage::Preparing => "正在准备文件…",
                            Stage::Dragging if snapshot.effect == COPY => "松开以复制到此处",
                            Stage::Dragging => "此处不能接收文件",
                            _ => "拖放已停止",
                        });
                    }
                    let mut dismiss = Vec::new();
                    for (index, ticket) in state.transfers.iter().enumerate() {
                        let snapshot = ticket.snapshot();
                        ui.horizontal(|ui| {
                            if snapshot.stage == Stage::Failed {
                                ui.label(snapshot.error.as_deref().unwrap_or("文件拖放失败"));
                                if ui.button("关闭").clicked() {
                                    dismiss.push(index);
                                }
                            } else {
                                ui.label(match snapshot.stage {
                                    Stage::Preparing => "正在准备文件…".to_owned(),
                                    Stage::Dragging if snapshot.effect == COPY => {
                                        "松开以复制到此处".to_owned()
                                    }
                                    Stage::Dragging => "此处不能接收文件".to_owned(),
                                    Stage::Returning => "正在交还原拖动…".to_owned(),
                                    Stage::Submitted => "等待目标接收…".to_owned(),
                                    _ => format!(
                                        "正在复制文件 · {:.1} MiB",
                                        snapshot.bytes_read as f64 / 1048576.
                                    ),
                                });
                                if ui.button("取消").clicked() {
                                    ticket.cancel();
                                }
                            }
                        });
                    }
                    for index in dismiss.into_iter().rev() {
                        state.transfers.remove(index);
                    }
                    if let Some(error) = state.error.clone() {
                        ui.horizontal(|ui| {
                            ui.label(error);
                            if ui.button("关闭").clicked() {
                                state.error = None;
                            }
                        });
                    }
                });
            });
    }
}
impl State {
    fn point(&self, point: POINTL) -> Option<Point> {
        let (screen, x, y) = self
            .geometry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?
            .position(
                POINT {
                    x: point.x,
                    y: point.y,
                },
                false,
            )?;
        Some(Point { screen, x, y })
    }
    fn cancel_preview(&mut self) {
        if let Some(ticket) = self.returning.take() {
            ticket.cancel();
        }
        if let Some(ticket) = self.preview.take() {
            ticket.cancel();
        }
    }
}
impl Handler for State {
    fn description(&self) -> Option<&'static str> {
        Some("复制到远端")
    }
    fn reenter(
        &mut self,
        identity: crate::platform::windows::drag_drop::appearance::Identity,
        point: POINTL,
    ) -> Option<u32> {
        if let Some(ticket) = &self.returning {
            return Some(
                self.point(point)
                    .map_or(0, |point| ticket.return_position(point)),
            );
        }
        // A marker from another connection is never interpreted as local files.
        let Some(ticket) = self.controller.returning(identity, self.owner) else {
            return Some(0);
        };
        let Some(point) = self.point(point) else {
            return Some(0);
        };
        let owner = self.owner;
        let hwnd = windows::Win32::Foundation::HWND(owner as _);
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                GetForegroundWindow, SetForegroundWindow,
            };
            if GetForegroundWindow() != hwnd && !SetForegroundWindow(hwnd).as_bool() {
                return Some(0);
            }
        }
        let geometry = self.geometry.clone();
        let position = Arc::new(move || {
            use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow};
            if unsafe { GetForegroundWindow().0 as u64 } != owner {
                return None;
            }
            let mut cursor = POINT::default();
            unsafe {
                GetCursorPos(&mut cursor).ok()?;
            }
            let target = geometry.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
            let (screen, x, y) = target.position(cursor, true)?;
            Some(Point { screen, x, y })
        });
        if let Err(error) = ticket.return_enter(point, position) {
            self.error = Some(error.to_string());
            return Some(0);
        }
        tracing::info!(
            owner,
            drag = identity.drag,
            "native drag reentry reached viewer video"
        );
        self.returning = Some(ticket);
        Some(0)
    }
    fn enter(&mut self, paths: Vec<std::path::PathBuf>, point: POINTL) -> u32 {
        self.paths = paths;
        self.error = None;
        self.over(point)
    }
    fn over(&mut self, point: POINTL) -> u32 {
        if let Some(ticket) = &self.returning {
            return self
                .point(point)
                .map_or(0, |point| ticket.return_position(point));
        }
        let Some(point) = self.point(point).filter(|_| self.controller.available()) else {
            self.cancel_preview();
            return 0;
        };
        if self.preview.is_none() && !self.paths.is_empty() {
            match self.controller.begin(self.paths.clone(), point) {
                Ok(ticket) => {
                    // End any ordinary input ownership before the remote OLE
                    // source takes over. This does not change saved control mode.
                    self.control.mouse().pause_owner(self.owner);
                    self.preview = Some(ticket);
                }
                Err(error) => {
                    self.error = Some(error.to_string());
                    return 0;
                }
            }
        }
        self.preview.as_ref().map_or(0, |ticket| {
            ticket.position(point);
            ticket.snapshot().effect & COPY
        })
    }
    fn leave(&mut self) {
        // Canceling our local OLE loop sends DragLeave. The actor owns the
        // outstanding original-gesture handoff; this callback must not cancel it.
        self.returning.take();
        if let Some(ticket) = self.preview.take() {
            ticket.cancel();
        }
        self.paths.clear();
    }
    fn drop_at(&mut self, point: POINTL) -> u32 {
        if let Some(ticket) = self.returning.take() {
            return match self
                .point(point)
                .ok_or_else(|| anyhow::anyhow!("拖回位置已失效"))
                .and_then(|point| ticket.return_commit(point))
            {
                Ok(()) => 0,
                Err(error) => {
                    ticket.cancel();
                    self.error = Some(error.to_string());
                    0
                }
            };
        }
        let Some(point) = self.point(point) else {
            self.leave();
            return 0;
        };
        self.paths.clear();
        let Some(ticket) = self.preview.take() else {
            return 0;
        };
        match ticket.commit(point) {
            Ok(()) => {
                self.transfers.push(ticket);
                COPY
            }
            Err(error) => {
                ticket.cancel();
                self.error = Some(error.to_string());
                0
            }
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        *self.geometry.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.cancel_preview();
        for ticket in self.controller.take_reverse(self.owner) {
            ticket.cancel();
        }
        for ticket in &self.transfers {
            ticket.cancel();
        }
    }
}
