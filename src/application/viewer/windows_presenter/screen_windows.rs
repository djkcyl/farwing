use super::*;
use crate::application::viewer::screens::ScreenPlayback;
use crate::features::stream_control::RemoteScreen;
use crate::i18n::t;
use std::collections::HashMap;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use winit::platform::windows::WindowAttributesExtWindows;

#[derive(Clone, Copy)]
enum TabCommand {
    Select(i32),
    Detach(i32, PhysicalPosition<i32>),
}

#[derive(Default)]
pub(super) struct ScreenTabBar {
    pub(super) device_switch: Option<super::super::device_switch::DeviceSwitcher>,
    pub tabs: Vec<(i32, String)>,
    selected: i32,
    pending: Option<i32>,
    error: Option<String>,
    command: Option<TabCommand>,
    software_only: bool,
}

impl ScreenTabBar {
    pub(super) fn is_pending(&self) -> bool {
        self.pending.is_some()
            || self
                .device_switch
                .as_ref()
                .is_some_and(|switcher| switcher.is_switching())
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, window: &Window, control: &StreamControlHandle) {
        let snapshot = control.snapshot();
        let width = ui.available_width();
        let mut row = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(ui.max_rect())
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        row.spacing_mut().item_spacing.x = 3.0;
        egui::ScrollArea::horizontal()
            .id_salt("screen-tabs")
            .auto_shrink([false, false])
            .max_width(width)
            // A drag either detaches a tab or moves the caption background.
            // Overflow stays accessible via the wheel and scroll bar.
            .scroll_source(
                egui::scroll_area::ScrollSource::SCROLL_BAR
                    | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
            )
            .show(&mut row, |ui| {
                ui.horizontal_centered(|ui| {
                    for (id, name) in &self.tabs {
                        let selected = self.selected == *id;
                        let pending =
                            self.pending == Some(*id) || control.display_change_pending(*id);
                        let closable = snapshot.ready
                            && snapshot.dpi_settings_supported
                            && snapshot.screens.len() > 1
                            && (!snapshot.topology.pending || snapshot.topology.dismissed)
                            && snapshot
                                .screens
                                .iter()
                                .any(|s| s.id == *id && s.display.screen_type == 1);
                        let (response, close_clicked) = ui
                            .push_id(("screen-tab", *id), |ui| {
                                crate::ui::controls::screen_tab(
                                    ui,
                                    name,
                                    selected,
                                    pending,
                                    !self.software_only,
                                    closable,
                                )
                            })
                            .inner;
                        let response = response.on_hover_text(name);
                        if close_clicked {
                            super::super::stream_menu::topology_menu::request(
                                ui.ctx(),
                                control,
                                *id,
                                crate::features::stream_control::DisplayTopologyAction::Remove {
                                    screen_id: *id,
                                },
                            );
                        } else if response.clicked() {
                            self.command = Some(TabCommand::Select(*id));
                        }
                        response.context_menu(|ui| {
                            super::super::stream_menu::menu_style(ui);
                            ui.set_width(crate::ui::theme::CONTEXT_MENU_WIDTH);
                            if let Some(error) = super::super::stream_menu::tab_display_menu(
                                ui,
                                control,
                                *id,
                                window
                                    .current_monitor()
                                    .map(|m| (m.size().width, m.size().height)),
                            ) {
                                self.error = Some(error);
                            }
                            ui.separator();
                            if crate::ui::controls::menu_row(
                                ui,
                                t().viewer.open_in_window,
                                "",
                                None,
                                !self.software_only,
                                false,
                            )
                            .on_disabled_hover_text(t().viewer.software_single_window)
                            .clicked()
                            {
                                if let Ok(origin) = window.outer_position() {
                                    self.command = Some(TabCommand::Detach(
                                        *id,
                                        PhysicalPosition::new(origin.x + 80, origin.y + 100),
                                    ));
                                }
                                ui.close();
                            }
                        });
                        if response.drag_started()
                            && let Ok(handle) = window.window_handle()
                            && let RawWindowHandle::Win32(handle) = handle.as_raw()
                        {
                            unsafe {
                                SetCapture(HWND(handle.hwnd.get() as _));
                            }
                        }
                        if response.drag_stopped() {
                            let mut point = POINT::default();
                            if unsafe { GetCursorPos(&mut point) }.is_ok() {
                                self.command = Some(TabCommand::Detach(
                                    *id,
                                    PhysicalPosition::new(point.x, point.y),
                                ));
                            }
                            let _ = unsafe { ReleaseCapture() };
                        }
                    }
                    let snapshot = control.snapshot();
                    if crate::features::stream_control::take_dev_virtual_screen(
                        snapshot.ready && snapshot.topology_support.create && !snapshot.topology.pending,
                    ) {
                        let (width, height, _) =
                            super::super::stream_menu::topology_menu::local_parameters(
                                ui.ctx(),
                                control,
                                window
                                    .current_monitor()
                                    .map(|m| (m.size().width, m.size().height)),
                            );
                        if let Err(error) = control.apply_display_topology(
                            self.selected,
                            crate::features::stream_control::DisplayTopologyAction::Create {
                                width,
                                height,
                            },
                        ) {
                            tracing::warn!(%error, "development virtual screen not added");
                        }
                    }
                    if snapshot.topology_support.create_visible
                        && crate::ui::controls::screen_add(
                            ui,
                            snapshot.ready
                                && snapshot.topology_support.create
                                && (!snapshot.topology.pending || snapshot.topology.dismissed),
                        )
                        .on_hover_text(t().controls.add_virtual_screen)
                        .on_disabled_hover_text(
                            snapshot
                                .topology_support
                                .create_unavailable
                                .unwrap_or(t().viewer.display_action_busy),
                        )
                        .clicked()
                    {
                        let (width, height, _) =
                            super::super::stream_menu::topology_menu::local_parameters(
                                ui.ctx(),
                                control,
                                window
                                    .current_monitor()
                                    .map(|m| (m.size().width, m.size().height)),
                            );
                        super::super::stream_menu::topology_menu::request(
                            ui.ctx(),
                            control,
                            self.selected,
                            crate::features::stream_control::DisplayTopologyAction::Create {
                                width,
                                height,
                            },
                        );
                    }
                });
            });
        if let Some(error) = self.error.clone() {
            egui::Modal::new(ui.id().with("screen-error"))
                .frame(crate::ui::controls::dialog_frame())
                .show(ui.ctx(), |ui| {
                    ui.set_width(300.0);
                    let close = crate::ui::controls::dialog_header(
                        ui,
                        t().viewer.display_action_failed,
                        crate::ui::controls::DialogIcon::Error,
                        true,
                    );
                    ui.label(error);
                    let accept = crate::ui::controls::dialog_actions(
                        ui,
                        Some(crate::ui::controls::DialogAction::new(t().common.ok)),
                        None,
                    )
                    .0;
                    if close || accept {
                        self.error = None;
                    }
                });
        }
    }
}

struct PendingScreen {
    id: i32,
    task: tokio::task::JoinHandle<()>,
    result: tokio::sync::oneshot::Receiver<Result<Arc<NativeViewerSession>>>,
    connecting: Option<WindowsConnectionApp>,
}
impl Drop for PendingScreen {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ScreenWindow {
    // Drop renderers before destroying their parent HWND.
    app: Option<ThreadedWindowsApp>,
    pending: Option<PendingScreen>,
    tabs: Vec<i32>,
    selected: Option<i32>,
    failed_screen: Option<(i32, i32, i32)>,
    close: bool,
    last_frame: Option<Instant>,
    repaint: Option<Instant>,
    interval: Duration,
    window: Window,
}

pub(super) struct ScreenWindows {
    windows: HashMap<WindowId, ScreenWindow>,
    sessions: HashMap<i32, Arc<NativeViewerSession>>,
    factory: ScreenPlayback,
    shutdown: Arc<AtomicBool>,
    proxy: EventLoopProxy<UiEvent>,
    generation: u64,
    next_refresh: Instant,
    catalog: Vec<RemoteScreen>,
    topology_sequence: Option<i64>,
    topology_owner: Option<WindowId>,
    topology_focused: Option<i32>,
}

impl ScreenWindows {
    pub fn owns(&self, id: WindowId) -> bool {
        self.windows.contains_key(&id)
    }
    pub fn focus(&self) {
        if let Some(slot) = self.windows.values().next() {
            slot.window.set_visible(true);
            slot.window.focus_window();
        }
    }
    pub fn new(
        window: Window,
        mut session: NativeViewerSession,
        connecting: WindowsConnectionApp,
        proxy: EventLoopProxy<UiEvent>,
        generation: u64,
    ) -> Result<Self> {
        let factory = session
            .take_screen_playback()
            .context("screen playback owner unavailable")?;
        let shutdown = Arc::clone(&session.shutdown);
        let screen_id = session.screen_id();
        let session = Arc::new(session);
        factory.register(Arc::clone(&session));
        let app = ThreadedWindowsApp::from_session(&window, Arc::clone(&session), connecting)?;
        let catalog = factory.screens();
        let tabs = catalog.iter().map(|screen| screen.id).collect();
        let slot = ScreenWindow {
            app: Some(app),
            pending: None,
            tabs,
            selected: Some(screen_id),
            failed_screen: None,
            close: false,
            last_frame: None,
            repaint: Some(Instant::now()),
            interval: ui_frame_interval(&window),
            window,
        };
        let mut group = Self {
            windows: HashMap::from([(slot.window.id(), slot)]),
            sessions: HashMap::from([(session.track_index, session)]),
            factory,
            shutdown,
            proxy,
            generation,
            next_refresh: Instant::now(),
            catalog,
            topology_sequence: None,
            topology_owner: None,
            topology_focused: None,
        };
        group.sync_bars();
        Ok(group)
    }

    /// The window `take_window(preferred)` keeps.
    fn kept(&self, preferred: Option<WindowId>) -> Option<WindowId> {
        preferred
            .filter(|id| self.windows.contains_key(id))
            .or_else(|| self.windows.keys().next().copied())
    }

    /// The picture in the window `take_window(preferred)` keeps.
    pub fn snapshot(&self, preferred: Option<WindowId>) -> Option<egui::ColorImage> {
        let slot = self.windows.get(&self.kept(preferred)?)?;
        slot.app.as_ref()?.renderer.snapshot()
    }

    pub fn take_window(&mut self, preferred: Option<WindowId>) -> Option<Window> {
        let id = self.kept(preferred)?;
        let mut slot = self.windows.remove(&id)?;
        if let Some(app) = slot.app.as_mut() {
            app.mouse.release(&slot.window);
        }
        slot.app.take();
        slot.pending.take();
        Some(slot.window)
    }

    pub fn window_event(
        &mut self,
        id: WindowId,
        event: &WindowEvent,
        event_loop: &ActiveEventLoop,
    ) {
        let Some(slot) = self.windows.get_mut(&id) else {
            return;
        };
        if *event == WindowEvent::CloseRequested {
            if let Some(app) = slot.app.as_mut() {
                app.mouse.release(&slot.window);
            }
            slot.close = true;
            return;
        }
        if matches!(
            event,
            WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. }
        ) {
            slot.interval = ui_frame_interval(&slot.window);
        }
        if *event == WindowEvent::Focused(true)
            && let Some(id) = slot.selected
        {
            self.factory.focus(id);
        }
        if *event == WindowEvent::RedrawRequested {
            if slot.window.is_minimized().unwrap_or(false) {
                slot.repaint = None;
                return;
            }
            if let Some(earliest) = slot.last_frame.map(|at| at + slot.interval)
                && Instant::now() < earliest
            {
                slot.repaint = Some(earliest);
                return;
            }
            slot.repaint = None;
            slot.last_frame = Some(Instant::now());
        }
        if let Some(app) = slot.app.as_mut() {
            if let Err(error) = app.on_window_event(&slot.window, event, event_loop) {
                tracing::error!(%error, "screen window presentation failed");
                app.screen_tabs.error = Some(format!("{error:#}"));
            }
        } else if let Some(app) = slot
            .pending
            .as_mut()
            .and_then(|pending| pending.connecting.as_mut())
        {
            if let Err(error) = app.on_window_event(&slot.window, event) {
                tracing::warn!(%error, "draw opening screen window failed");
                slot.close = true;
            }
            slot.close |= app.close_requested;
        }
    }

    pub fn repaint(&mut self, event: UiRepaintEvent) {
        if event.generation != self.generation {
            return;
        }
        let Some(slot) = self.windows.get_mut(&event.window) else {
            return;
        };
        let context = slot.app.as_ref().map(|app| &app.egui_context).or_else(|| {
            slot.pending
                .as_ref()
                .and_then(|p| p.connecting.as_ref())
                .map(|app| &app.egui_context)
        });
        if let Some(context) = context {
            let pass = context.cumulative_pass_nr();
            if pass == event.pass || pass == event.pass.saturating_add(1) {
                let interval = if slot.app.is_some() {
                    slot.interval
                } else {
                    slot.interval.max(super::connection::WAITING_FRAME_INTERVAL)
                };
                let when = slot
                    .last_frame
                    .map_or(event.when, |at| event.when.max(at + interval));
                slot.repaint = Some(slot.repaint.map_or(when, |old| old.min(when)));
            }
        }
    }

    fn begin_screen(&mut self, window: WindowId, screen_id: i32) -> Result<()> {
        let switch_started = Instant::now();
        tracing::debug!(screen_id, "screen switch begin");
        let screen = self
            .catalog
            .iter()
            .find(|screen| screen.id == screen_id)
            .context(t().viewer.display_disconnected)?;
        let slot = self
            .windows
            .get_mut(&window)
            .context(t().viewer.window_closed)?;
        slot.failed_screen = None;
        if slot.selected == Some(screen_id) && slot.app.is_some() {
            slot.pending.take();
            if let Some(app) = slot.app.as_mut() {
                app.screen_tabs.pending = None;
            }
            return Ok(());
        }
        if slot
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == screen_id)
        {
            return Ok(());
        }
        let cancelled_pending = slot.pending.take().map(|pending| pending.id);
        if let Some(app) = slot.app.as_ref() {
            app.stream_control
                .cancel_display_change(app._session.screen_id());
        }
        if let Some(app) = slot.app.as_mut() {
            app.mouse.release(&slot.window);
        }
        let (progress, receiver) = std_mpsc::channel();
        let _ = progress.send(ConnectionProgress::working(
            10,
            t().viewer.opening_display,
            screen.label(&self.catalog),
        ));
        let (connecting, display) = if slot.app.is_some() {
            (
                None,
                // Cached tracks need no new device. New decoders choose their
                // device on the decoder worker, outside the UI event thread.
                ViewerDisplayHandle::default(),
            )
        } else {
            let (mut connecting, display) = WindowsConnectionApp::new(
                &slot.window,
                t().viewer.display.into(),
                receiver,
                self.proxy.clone(),
                self.generation,
            )?;
            connecting.render(&slot.window)?;
            (Some(connecting), display)
        };
        tracing::debug!(
            screen_id,
            elapsed_ms = switch_started.elapsed().as_secs_f64() * 1000.0,
            "screen switch display prepared"
        );
        let repaint = slot
            .app
            .as_ref()
            .map(|app| app.egui_context.clone())
            .or_else(|| connecting.as_ref().map(|app| app.egui_context.clone()))
            .expect("screen window UI context");
        let (task, result) = self.factory.open(
            screen_id,
            display,
            slot.selected,
            cancelled_pending,
            move || repaint.request_repaint(),
        );
        slot.pending = Some(PendingScreen {
            id: screen_id,
            task,
            result,
            connecting,
        });
        if let Some(app) = slot.app.as_mut() {
            app.screen_tabs.pending = Some(screen_id);
        }
        slot.window.request_redraw();
        Ok(())
    }

    fn close_window(&mut self, id: WindowId) {
        let Some(mut slot) = self.windows.remove(&id) else {
            return;
        };
        slot.window.set_visible(false);
        slot.pending.take();
        if let Some(app) = slot.app.as_mut() {
            app.stream_control
                .cancel_display_change(app._session.screen_id());
            app.mouse.release(&slot.window);
        }
        slot.app.take();
        if let Some(target) = self.windows.values_mut().next() {
            for screen in slot.tabs {
                if !target.tabs.contains(&screen) {
                    target.tabs.push(screen);
                }
            }
            target.window.request_redraw();
        }
    }

    fn detach_tab(
        &mut self,
        event_loop: &ActiveEventLoop,
        from: WindowId,
        screen: i32,
        at: PhysicalPosition<i32>,
    ) -> Result<()> {
        let source = self
            .windows
            .get(&from)
            .context(t().viewer.source_window_closed)?;
        if !source.tabs.contains(&screen) {
            return Ok(());
        }
        if let Ok(origin) = source.window.inner_position()
            && at.x >= origin.x
            && at.x < origin.x + source.window.inner_size().width as i32
            && at.y >= origin.y
            && at.y < origin.y + title_bar_height_pixels(&source.window) as i32
        {
            return Ok(());
        }
        let position = crate::ui::chrome::constrain_drag_position(
            PhysicalPosition::new(at.x - 100, at.y - 18),
            windows::Win32::Foundation::POINT { x: at.x, y: at.y },
        );
        if source.tabs.len() == 1 {
            source.window.set_outer_position(position);
            return Ok(());
        }
        if source
            .app
            .as_ref()
            .is_some_and(|app| app._session.is_software())
        {
            bail!(t().viewer.software_single_window);
        }
        let target = {
            let window = event_loop.create_window(
                WindowAttributes::default()
                    .with_drag_and_drop(false)
                    .with_visible(false)
                    .with_title(source.window.title())
                    .with_window_icon(Some(crate::ui::branding::window_icon()))
                    .with_decorations(false)
                    .with_inner_size(source.window.inner_size())
                    .with_min_inner_size(LogicalSize::new(760.0, 520.0))
                    .with_position(position),
            )?;
            crate::ui::branding::set_taskbar_icon(&window);
            configure_dwm_window(&window);
            let id = window.id();
            self.windows.insert(
                id,
                ScreenWindow {
                    interval: ui_frame_interval(&window),
                    window,
                    app: None,
                    pending: None,
                    tabs: Vec::new(),
                    selected: None,
                    failed_screen: None,
                    close: false,
                    last_frame: None,
                    repaint: Some(Instant::now()),
                },
            );
            id
        };
        self.windows
            .get_mut(&target)
            .expect("target exists")
            .tabs
            .push(screen);
        if let Err(error) = self.begin_screen(target, screen) {
            if let Some(slot) = self.windows.get_mut(&target) {
                slot.tabs.retain(|id| *id != screen);
            }
            if self
                .windows
                .get(&target)
                .is_some_and(|slot| slot.tabs.is_empty())
            {
                self.close_window(target);
            }
            return Err(error);
        }
        let source = self.windows.get_mut(&from).expect("source exists");
        source.tabs.retain(|id| *id != screen);
        if source.selected == Some(screen) {
            source.app.take();
            source.selected = None;
        }
        if source
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == screen)
        {
            source.pending.take();
        }
        if let Some(next) = self
            .windows
            .get(&from)
            .filter(|slot| slot.selected.is_none())
            .and_then(|slot| slot.tabs.first())
            .copied()
        {
            self.begin_screen(from, next)?;
        }
        if self
            .windows
            .get(&from)
            .is_some_and(|slot| slot.tabs.is_empty())
        {
            self.close_window(from);
        }
        Ok(())
    }

    fn sync_bars(&mut self) {
        for slot in self.windows.values_mut() {
            if let Some(app) = slot.app.as_mut() {
                app.screen_tabs.device_switch = self.factory.device_switch.clone();
                app.screen_tabs.software_only = app._session.is_software();
                app.screen_tabs.tabs = slot
                    .tabs
                    .iter()
                    .filter_map(|id| self.catalog.iter().find(|s| s.id == *id))
                    .map(|screen| (screen.id, screen.label(&self.catalog)))
                    .collect();
                app.screen_tabs.selected = slot.selected.unwrap_or(-1);
                app.screen_tabs.pending = slot.pending.as_ref().map(|pending| pending.id);
            }
        }
        let mut visible: Vec<_> = self
            .windows
            .values()
            .flat_map(|slot| {
                slot.selected
                    .filter(|_| {
                        !(slot.pending.is_some()
                            && slot
                                .app
                                .as_ref()
                                .is_some_and(|app| app._session.is_software()))
                    })
                    .into_iter()
                    .chain(slot.pending.as_ref().map(|pending| pending.id))
            })
            .collect();
        visible.sort_unstable();
        visible.dedup();
        self.factory.set_visible(visible);
    }

    fn refresh_catalog(&mut self) {
        let topology = self.factory.topology();
        if topology.sequence != self.topology_sequence {
            self.topology_sequence = topology.sequence;
            self.topology_focused = None;
            self.topology_owner = self
                .windows
                .iter()
                .find(|(_, s)| s.tabs.contains(&topology.origin_screen))
                .or_else(|| self.windows.iter().find(|(_, s)| s.window.has_focus()))
                .map(|(id, _)| *id);
        }
        let actual = self.factory.screens();
        self.catalog = actual.clone();
        for screen in &topology.transient_screens {
            if !self.catalog.iter().any(|s| s.id == screen.id) {
                self.catalog.push(screen.clone());
            }
        }
        if self.catalog.is_empty() {
            return;
        }
        let owner = self
            .topology_owner
            .filter(|id| self.windows.contains_key(id))
            .or_else(|| {
                self.windows
                    .iter()
                    .find(|(_, s)| s.window.has_focus())
                    .map(|(id, _)| *id)
            })
            .or_else(|| self.windows.keys().next().copied());
        for slot in self.windows.values_mut() {
            slot.tabs
                .retain(|id| self.catalog.iter().any(|screen| screen.id == *id));
            if slot
                .pending
                .as_ref()
                .is_some_and(|p| !actual.iter().any(|s| s.id == p.id))
            {
                slot.pending.take();
            }
        }
        let assigned: Vec<_> = self
            .windows
            .values()
            .flat_map(|s| s.tabs.iter().copied())
            .collect();
        if let Some(slot) = owner.and_then(|id| self.windows.get_mut(&id)) {
            slot.tabs.extend(
                self.catalog
                    .iter()
                    .filter(|s| !assigned.contains(&s.id))
                    .map(|s| s.id),
            );
        }
        let mut topology_selection = None;
        if self.topology_focused != topology.selected_screen
            && topology.reported
            && !matches!(
                topology.action,
                Some(crate::features::stream_control::DisplayTopologyAction::Remove { .. })
            )
            && let Some(target) = topology
                .selected_screen
                .filter(|id| actual.iter().any(|s| s.id == *id))
            && let Some(owner) = owner
        {
            for (id, slot) in &mut self.windows {
                if *id != owner {
                    slot.tabs.retain(|id| *id != target);
                }
            }
            if let Some(slot) = self.windows.get_mut(&owner) {
                if !slot.tabs.contains(&target) {
                    slot.tabs.push(target);
                }
                if slot
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.id != target)
                {
                    slot.pending.take();
                }
                slot.failed_screen = None;
            }
            // Select the new source explicitly without changing the tab order.
            topology_selection = Some((owner, target));
            self.topology_focused = Some(target);
        }
        let empty: Vec<_> = self
            .windows
            .iter()
            .filter(|(_, slot)| slot.tabs.is_empty())
            .map(|(id, _)| *id)
            .collect();
        for id in empty {
            if self.windows.len() > 1 {
                self.close_window(id);
            }
        }
        let replacements: Vec<_> = self
            .windows
            .iter_mut()
            .filter_map(|(id, slot)| {
                if slot.pending.is_some() {
                    return None;
                }
                let selected = topology_selection
                    .filter(|(window, _)| window == id)
                    .map(|(_, screen)| screen)
                    .or(slot.selected)
                    .and_then(|id| self.catalog.iter().find(|s| s.id == id));
                if selected.is_some_and(|s| !actual.iter().any(|real| real.id == s.id)) {
                    return None;
                }
                let target = match selected {
                    Some(screen)
                        if slot.app.as_ref().is_some_and(|app| {
                            app._session.track_index == screen.video_track_index
                                && app.bound_screen
                                    == (screen.id as u32 as u64
                                        | ((screen.display.screen_type as u32 as u64) << 32))
                        }) =>
                    {
                        return None;
                    }
                    Some(screen) => Some(screen.id),
                    None => slot
                        .tabs
                        .iter()
                        .find(|id| actual.iter().any(|s| s.id == **id))
                        .copied(),
                };
                if target.is_some_and(|target| {
                    actual.iter().any(|s| {
                        s.id == target
                            && slot.failed_screen
                                == Some((s.id, s.video_track_index, s.display.screen_type))
                    })
                }) {
                    return None;
                }
                if let Some(app) = slot.app.as_mut() {
                    app.mouse.release(&slot.window);
                }
                slot.selected = None;
                target.map(|target| (*id, target))
            })
            .collect();
        for (window, screen) in replacements {
            if let Err(error) = self.begin_screen(window, screen)
                && let Some(slot) = self.windows.get_mut(&window)
            {
                slot.failed_screen = self
                    .catalog
                    .iter()
                    .find(|s| s.id == screen)
                    .map(|s| (s.id, s.video_track_index, s.display.screen_type));
                if let Some(app) = slot.app.as_mut() {
                    app.screen_tabs.error = Some(error.to_string());
                }
            }
        }
    }

    pub fn update(&mut self, event_loop: &ActiveEventLoop) -> bool {
        if self.shutdown.load(Ordering::Acquire) || self.windows.is_empty() {
            return false;
        }
        let ids: Vec<_> = self.windows.keys().copied().collect();
        for id in &ids {
            let slot = self.windows.get_mut(id).expect("window exists");
            if slot.app.as_ref().is_some_and(|app| app.close_requested) || slot.close {
                self.close_window(*id);
                continue;
            }
            let ready = slot
                .pending
                .as_mut()
                .and_then(|pending| match pending.result.try_recv() {
                    Ok(result) => Some(result),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        Some(Err(anyhow!(t().viewer.screen_load_cancelled)))
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
                });
            if let Some(result) = ready {
                let mut pending = slot.pending.take().expect("pending screen");
                match result {
                    Ok(session) => {
                        if !self.factory.screens().iter().any(|s| {
                            s.id == pending.id
                                && s.video_track_index == session.track_index
                                && session.screen_binding()
                                    == (s.id as u32 as u64
                                        | ((s.display.screen_type as u32 as u64) << 32))
                        }) {
                            self.next_refresh = Instant::now();
                            continue;
                        }
                        tracing::debug!(screen_id = pending.id, "screen switch ready on UI");
                        let connected = if let Some(app) = slot.app.as_mut() {
                            app.replace_session(&slot.window, Arc::clone(&session))
                        } else {
                            ThreadedWindowsApp::from_session(
                                &slot.window,
                                Arc::clone(&session),
                                pending.connecting.take().expect("screen UI"),
                            )
                            .map(|app| {
                                slot.app = Some(app);
                            })
                        };
                        match connected {
                            Ok(()) => {
                                self.sessions.insert(session.track_index, session);
                                slot.selected = Some(pending.id);
                                self.factory.focus(pending.id);
                                tracing::debug!(screen_id = pending.id, "screen switch bound");
                            }
                            Err(error) => {
                                tracing::error!(%error, "initialize screen presenter failed");
                                slot.close = true;
                            }
                        }
                    }
                    Err(error) => {
                        slot.failed_screen = Some(
                            self.catalog
                                .iter()
                                .find(|s| s.id == pending.id)
                                .map_or((pending.id, -1, -1), |s| {
                                    (s.id, s.video_track_index, s.display.screen_type)
                                }),
                        );
                        if let Some(app) = slot.app.as_mut() {
                            app.screen_tabs.error = Some(format!("{error:#}"));
                        } else {
                            tracing::warn!(%error, "open detached screen failed");
                            slot.close = true;
                        }
                    }
                }
            }
        }
        let commands: Vec<_> = self
            .windows
            .iter_mut()
            .filter_map(|(id, slot)| {
                slot.app
                    .as_mut()
                    .and_then(|app| app.screen_tabs.command.take())
                    .map(|command| (*id, command))
            })
            .collect();
        for (window, command) in commands {
            let result = match command {
                TabCommand::Select(screen) => self.begin_screen(window, screen),
                TabCommand::Detach(screen, at) => self.detach_tab(event_loop, window, screen, at),
            };
            if let Err(error) = result
                && let Some(app) = self
                    .windows
                    .get_mut(&window)
                    .and_then(|slot| slot.app.as_mut())
            {
                app.screen_tabs.error = Some(format!("{error:#}"));
            }
        }
        let now = Instant::now();
        if now >= self.next_refresh {
            self.refresh_catalog();
            self.next_refresh = now + Duration::from_millis(250);
            for slot in self.windows.values() {
                slot.window.request_redraw();
            }
        }
        self.sync_bars();
        let mut wake = self.next_refresh;
        for slot in self.windows.values_mut() {
            if !slot.window.has_focus()
                && let Some(app) = slot.app.as_mut()
            {
                app.refresh_mouse(&slot.window, event_loop);
            }
            if let Some(at) = slot.repaint {
                if at <= now {
                    slot.window.request_redraw();
                    slot.repaint = None;
                } else {
                    wake = wake.min(at);
                }
            }
        }
        // Retire old owners first; only the foreground window installs input
        // and cursor presentation, regardless of HashMap iteration order.
        for slot in self.windows.values_mut() {
            if slot.window.has_focus()
                && let Some(app) = slot.app.as_mut()
            {
                app.refresh_mouse(&slot.window, event_loop);
            }
        }
        if self.windows.is_empty() {
            false
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
            true
        }
    }
}

impl Drop for ScreenWindows {
    fn drop(&mut self) {
        for slot in self.windows.values_mut() {
            slot.window.set_visible(false);
            if let Some(app) = slot.app.as_mut() {
                app.mouse.release(&slot.window);
            }
        }
        for session in self.sessions.values() {
            session.close_handle().close();
        }
        self.windows.clear();
        self.sessions.clear();
        self.factory.close_decoders();
    }
}
