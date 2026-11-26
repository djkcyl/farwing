//! Viewer caption and window geometry.
use super::screen_windows::ScreenTabBar;
use crate::application::viewer::StreamControlUi;
use crate::application::viewer_shortcuts::Action as ViewerShortcut;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::StreamControlHandle;
use crate::platform::graphics::window_hwnd;
use crate::ui::chrome::{
    WindowMoveState, WindowResizeState, handle_title_drag, paint_brand_logo,
    title_bar_height_pixels, update_nonmodal_window_move, update_nonmodal_window_resize,
    window_buttons,
};
use crate::ui::controls::{
    ViewerCaptionIcon as TitleIcon, viewer_caption_button as title_icon_button,
};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use winit::dpi::PhysicalSize;
use winit::window::{ResizeDirection, Window};

pub(in crate::application::viewer) struct PlayerTitleBar<'a> {
    pub(in crate::application::viewer) screens: &'a mut ScreenTabBar,
    pub(in crate::application::viewer) window: &'a Window,
    pub(in crate::application::viewer) title: &'a str,
    pub(in crate::application::viewer) performance: &'a PerformanceMonitor,
    pub(in crate::application::viewer) stream_control: &'a StreamControlHandle,
    pub(in crate::application::viewer) stream_control_ui: &'a mut StreamControlUi,
    pub(in crate::application::viewer) plugin_menu_open: &'a mut bool,
    pub(in crate::application::viewer) move_state: Option<&'a mut WindowMoveState>,
}

pub(in crate::application::viewer) fn player_title_bar(
    ui: &mut egui::Ui,
    mut bar: PlayerTitleBar<'_>,
) -> PlayerChromeAction {
    ui.set_min_height(crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT);
    let mut action = PlayerChromeAction::default();
    let stats = bar.performance.snapshot();
    let rect = egui::Rect::from_min_size(
        ui.available_rect_before_wrap().min,
        egui::vec2(
            ui.available_width(),
            crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT,
        ),
    );
    const VIEW_ACTIONS_WIDTH: f32 = crate::ui::theme::VIEWER_ACTIONS_WIDTH;
    let title = bar.title.trim_start_matches(crate::VIEWER_TITLE_PREFIX);
    let title_width = if bar.screens.device_switch.is_some() {
        crate::ui::controls::viewer_device_button_width(ui, title)
    } else {
        ui.painter()
            .layout_no_wrap(
                title.into(),
                egui::FontId::proportional(crate::ui::theme::BODY),
                crate::ui::theme::TEXT,
            )
            .size()
            .x
    };
    let identity_width =
        (crate::ui::theme::WINDOW_LOGO_SIZE + ui.spacing().item_spacing.x + title_width)
            .min(crate::ui::theme::VIEWER_IDENTITY_MAX_WIDTH);
    let controls_rect = egui::Rect::from_min_max(
        egui::pos2(
            rect.max.x - crate::ui::theme::WINDOW_CONTROLS_WIDTH,
            rect.min.y,
        ),
        rect.max,
    );
    let actions_rect = egui::Rect::from_min_max(
        egui::pos2(controls_rect.min.x - VIEW_ACTIONS_WIDTH, rect.min.y),
        egui::pos2(controls_rect.min.x, rect.max.y),
    );
    let identity_rect = egui::Rect::from_min_size(
        rect.min,
        egui::vec2(
            identity_width.min((actions_rect.min.x - rect.min.x).max(0.0)),
            rect.height(),
        ),
    );
    let content_rect = egui::Rect::from_min_max(
        egui::pos2(
            identity_rect.max.x + crate::ui::theme::VIEWER_IDENTITY_GAP,
            rect.min.y,
        ),
        egui::pos2(actions_rect.min.x, rect.max.y),
    );
    let metrics_rect = egui::Rect::from_min_max(
        egui::pos2(
            (content_rect.max.x - 184.0).max(content_rect.min.x),
            rect.min.y,
        ),
        content_rect.max,
    );
    let tabs_rect =
        egui::Rect::from_min_max(content_rect.min, egui::pos2(metrics_rect.min.x, rect.max.y));

    // Register the caption background before the tabs. Actual tab buttons
    // take priority for detach gestures; unused tab space and stream metrics
    // remain part of the draggable caption. Keep action buttons outside it.
    let drag = ui.interact(
        egui::Rect::from_min_max(
            egui::pos2(
                if bar.screens.device_switch.is_some() {
                    identity_rect.max.x
                } else {
                    rect.min.x
                },
                rect.min.y,
            ),
            metrics_rect.max,
        ),
        ui.id().with("player-title-drag"),
        egui::Sense::click_and_drag(),
    );
    if bar.window.fullscreen().is_some() {
        // Floating fullscreen chrome must not move/maximize the playback window.
    } else if let Some(state) = bar.move_state.as_deref_mut() {
        update_nonmodal_window_move(ui.ctx(), bar.window, &drag, state);
    } else {
        action.drag_window = handle_title_drag(bar.window, &drag);
    }

    let mut identity = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(identity_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    identity.set_clip_rect(identity_rect);
    paint_brand_logo(&mut identity);
    if let Some(switcher) = &bar.screens.device_switch {
        let response = crate::ui::controls::viewer_device_button(
            &mut identity,
            bar.title.trim_start_matches(crate::VIEWER_TITLE_PREFIX),
        );
        switcher.menu(&response, bar.window.id());
    } else {
        identity.add(
            egui::Label::new(
                egui::RichText::new(bar.title.trim_start_matches(crate::VIEWER_TITLE_PREFIX))
                    .size(crate::ui::theme::BODY)
                    .strong()
                    .color(crate::ui::theme::TEXT),
            )
            .truncate(),
        );
    }

    crate::ui::controls::viewer_caption_separator(
        ui,
        egui::pos2(
            identity_rect.right() + crate::ui::theme::VIEWER_IDENTITY_GAP / 2.0,
            rect.center().y,
        ),
    );

    if !bar.screens.tabs.is_empty() {
        let mut tabs = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(tabs_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        tabs.set_clip_rect(tabs_rect);
        bar.screens.draw(&mut tabs, bar.window, bar.stream_control);
    }
    let codec_info = if stats.video_format.contains("264") || stats.video_format.contains("265") {
        &stats.video_format
    } else {
        &stats.video_codec
    };
    let codec = if codec_info.contains("AV1") {
        "AV1"
    } else if codec_info.contains("265") || codec_info.contains("HEVC") {
        "H.265"
    } else if codec_info.contains("264") || codec_info.contains("AVC") {
        "H.264"
    } else {
        "—"
    };
    let parameters = stats.decoded_resolution.map_or_else(
        || "等待画面".to_owned(),
        |(width, height)| {
            format!(
                "{width}×{height} · {:.0} FPS · {codec}",
                stats.actual_fps.max(1.0)
            )
        },
    );
    let mut metrics = ui.new_child(egui::UiBuilder::new().max_rect(metrics_rect).layout(
        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
    ));
    metrics.set_clip_rect(metrics_rect);
    metrics
        .add(
            egui::Label::new(
                egui::RichText::new(parameters)
                    .size(crate::ui::theme::TINY)
                    .color(crate::ui::theme::MUTED),
            )
            .truncate(),
        )
        .on_hover_text(&stats.video_format);

    let mut actions = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(actions_rect.shrink2(egui::vec2(4.0, 0.0)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    actions.spacing_mut().item_spacing.x = 4.0;
    // Rightmost, so it stays reachable without a keyboard; the Linux player
    // has the same button in the same place.
    let fullscreen = bar.window.fullscreen().is_some();
    action.toggle_fullscreen = title_icon_button(
        &mut actions,
        if fullscreen {
            TitleIcon::ExitFullscreen
        } else {
            TitleIcon::Fullscreen
        },
        fullscreen,
        if fullscreen { "退出全屏" } else { "全屏" },
    )
    .clicked();
    let quality_button = title_icon_button(
        &mut actions,
        TitleIcon::Quality,
        bar.stream_control_ui.open,
        "画质与串流设置",
    );
    if quality_button.clicked() {
        bar.stream_control_ui.open = !bar.stream_control_ui.open;
        if bar.stream_control_ui.open {
            *bar.plugin_menu_open = false;
        }
    }
    if title_icon_button(
        &mut actions,
        TitleIcon::Plugins,
        *bar.plugin_menu_open,
        "插件",
    )
    .clicked()
    {
        *bar.plugin_menu_open = !*bar.plugin_menu_open;
        if *bar.plugin_menu_open {
            bar.stream_control_ui.open = false;
        }
    }
    let annotation = bar.stream_control.annotation_snapshot();
    action.toggle_annotation = actions
        .add_enabled_ui(
            !annotation.toggling
                && (annotation.enabled
                    || annotation.supported
                    || bar.stream_control.snapshot().ready
                        && bar.stream_control.remote_upgrade().is_some()),
            |ui| title_icon_button(ui, TitleIcon::Annotation, annotation.enabled, "批注"),
        )
        .inner
        .on_disabled_hover_text("当前设备暂不支持批注，或仍在连接中")
        .clicked();
    action.one_to_one = actions
        .add_enabled_ui(
            !bar.window.is_maximized() && bar.window.fullscreen().is_none(),
            |ui| {
                title_icon_button(
                    ui,
                    TitleIcon::OneToOne,
                    false,
                    "1:1 实际像素（固定左上角，空间不足时等比缩小）",
                )
            },
        )
        .inner
        .on_disabled_hover_text("请先还原窗口")
        .clicked();
    let control = bar.stream_control.snapshot();
    let mic = bar.stream_control.microphone().snapshot();
    let mic_hint = if mic.pending {
        "麦克风：等待远端确认"
    } else if let Some(error) = mic.error.as_deref() {
        error
    } else if mic.capturing {
        "正在将麦克风发送到远端，点击关闭"
    } else if mic.enabled {
        "麦克风已开启，等待远端应用使用，点击关闭"
    } else {
        "将所选麦克风发送到远端（设备可在高级设置中切换）"
    };

    // 动作区是从右往左排的，先加的落在右边，所以这里的代码顺序和屏幕上的左右
    // 顺序相反：剪贴板写在麦克风前头，显示出来才在麦克风右边。
    // 剪贴板按设备记住开没开，顶栏只是让你不用钻进串流设置的高级页。
    let clip = bar.stream_control.clipboard().snapshot();
    let clip_hint = if !clip.enabled {
        "同步剪贴板：把本机剪贴板发送到远端，并把远端剪贴板取回本机".to_owned()
    } else if let Some(error) = clip.error.as_deref() {
        error.to_owned()
    } else if clip.active {
        "剪贴板同步中，点击关闭".to_owned()
    } else {
        "剪贴板同步已开启，等待通道就绪，点击关闭".to_owned()
    };
    if actions
        .add_enabled_ui(
            clip.enabled
                || (control.ready
                    && control.mouse_mode != crate::features::remote_input::MouseMode::View),
            |ui| title_icon_button(ui, TitleIcon::Clipboard, clip.enabled, &clip_hint),
        )
        .inner
        .on_disabled_hover_text("请先连接并开启键鼠控制")
        .clicked()
    {
        crate::ui::controls::clear_notice(actions.ctx(), "clipboard-request-error");
        match bar.stream_control.set_clipboard_enabled(!clip.enabled) {
            Ok(()) => crate::ui::controls::clear_notice(actions.ctx(), "clipboard-error"),
            Err(error) => crate::ui::controls::notice(
                actions.ctx(),
                "clipboard-request-error",
                "剪贴板同步",
                crate::ui::controls::DialogIcon::Error,
                error.to_string(),
            ),
        }
    }

    if actions
        .add_enabled_ui(
            mic.enabled
                || (bar.stream_control.microphone_available()
                    && control.ready
                    && control.mouse_mode != crate::features::remote_input::MouseMode::View),
            |ui| title_icon_button(ui, TitleIcon::Microphone, mic.enabled, mic_hint),
        )
        .inner
        .on_disabled_hover_text("请先连接支持麦克风的 Windows 设备并开启键鼠控制")
        .clicked()
    {
        crate::ui::controls::clear_notice(actions.ctx(), "microphone-request-error");
        match bar.stream_control.set_microphone_enabled(!mic.enabled) {
            // Reset after acceptance so an identical rejection arriving before
            // the next frame is shown again. A local rejection must not re-arm
            // an old asynchronous error that is still in the microphone state.
            Ok(()) => crate::ui::controls::clear_notice(actions.ctx(), "microphone-error"),
            Err(error) => crate::ui::controls::notice(
                actions.ctx(),
                "microphone-request-error",
                "麦克风",
                crate::ui::controls::DialogIcon::Error,
                error.to_string(),
            ),
        }
    }
    let enabled = control.mouse_mode != crate::features::remote_input::MouseMode::View
        || control.mouse_pending;
    action.toggle_mouse = actions
        .add_enabled_ui(enabled || control.ready, |ui| {
            let hint = if bar.stream_control.mouse().transport_recovering() {
                "键鼠控制正在自动恢复；点击取消".into()
            } else if bar.stream_control.mouse().waiting_for_neutral() {
                format!(
                    "等待松开全部键鼠；退出控制快捷键：{}",
                    crate::application::viewer_shortcuts::label(ViewerShortcut::ReleaseMouse)
                )
            } else if enabled {
                format!(
                    "退出控制（{}）",
                    crate::application::viewer_shortcuts::label(ViewerShortcut::ReleaseMouse)
                )
            } else {
                if bar.stream_control.mouse().keyboard_supported() {
                    "开启键鼠控制".into()
                } else {
                    "开启鼠标控制（此平台暂未适配键盘）".into()
                }
            };

            title_icon_button(ui, TitleIcon::Mouse, enabled, &hint)
        })
        .inner
        .clicked();

    let mut controls = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(controls_rect.shrink2(egui::vec2(4.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    controls.spacing_mut().item_spacing.x = 4.0;
    if bar.window.fullscreen().is_some() {
        if title_icon_button(&mut controls, TitleIcon::Minimize, false, "最小化").clicked() {
            bar.window.set_minimized(true);
        }
        action.exit_fullscreen =
            title_icon_button(&mut controls, TitleIcon::Restore, false, "退出全屏").clicked();
        action.close =
            title_icon_button(&mut controls, TitleIcon::Close, false, "关闭当前串流窗口").clicked();
    } else {
        action.close = window_buttons(&mut controls, bar.window);
    }
    ui.painter().line_segment(
        [
            egui::pos2(controls_rect.min.x, rect.min.y + 8.0),
            egui::pos2(controls_rect.min.x, rect.max.y - 8.0),
        ],
        egui::Stroke::new(1.0, crate::ui::theme::LINE),
    );
    ui.advance_cursor_after_rect(rect);
    action
}

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::application::viewer) struct PlayerChromeAction {
    pub(in crate::application::viewer) exit_fullscreen: bool,
    pub(in crate::application::viewer) toggle_fullscreen: bool,
    pub(in crate::application::viewer) close: bool,
    pub(in crate::application::viewer) one_to_one: bool,
    pub(in crate::application::viewer) toggle_mouse: bool,
    pub(in crate::application::viewer) toggle_annotation: bool,
    pub(in crate::application::viewer) drag_window: bool,
}

pub(in crate::application::viewer) fn monitor_work_area(window: &Window) -> Option<RECT> {
    let hwnd = window_hwnd(window).ok()?;
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.0.is_null() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut info) }
        .as_bool()
        .then_some(info.rcWork)
}

pub(in crate::application::viewer) fn constrain_window_aspect(
    window: &Window,
    size: PhysicalSize<u32>,
    video_size: Option<(u32, u32)>,
    last_window_size: &mut PhysicalSize<u32>,
    pending_aspect_size: &mut Option<PhysicalSize<u32>>,
) {
    if size.width == 0 || size.height == 0 || window.is_maximized() || window.fullscreen().is_some()
    {
        *last_window_size = size;
        return;
    }
    if *pending_aspect_size == Some(size) {
        *pending_aspect_size = None;
        *last_window_size = size;
        return;
    }
    let Some((video_width, video_height)) = video_size else {
        *last_window_size = size;
        return;
    };
    let title_height = title_bar_height_pixels(window);
    let width_changed = size.width.abs_diff(last_window_size.width);
    let height_changed = size.height.abs_diff(last_window_size.height);
    let desired = if width_changed >= height_changed {
        PhysicalSize::new(
            size.width,
            ((u64::from(size.width) * u64::from(video_height) + u64::from(video_width) / 2)
                / u64::from(video_width)) as u32
                + title_height,
        )
    } else {
        let content_height = size.height.saturating_sub(title_height).max(1);
        PhysicalSize::new(
            ((u64::from(content_height) * u64::from(video_width) + u64::from(video_height) / 2)
                / u64::from(video_height)) as u32,
            size.height,
        )
    };
    *last_window_size = size;
    if desired.width.abs_diff(size.width) > 1 || desired.height.abs_diff(size.height) > 1 {
        *pending_aspect_size = Some(desired);
        let _ = window.request_inner_size(desired);
    }
}

pub(in crate::application::viewer) fn fit_window_to_aspect(
    window: &Window,
    (video_width, video_height): (u32, u32),
    pending_aspect_size: &mut Option<PhysicalSize<u32>>,
) {
    if window.is_maximized() || window.fullscreen().is_some() {
        return;
    }
    let current = window.inner_size();
    let title_height = title_bar_height_pixels(window);
    let mut desired = PhysicalSize::new(
        current.width,
        ((u64::from(current.width) * u64::from(video_height) + u64::from(video_width) / 2)
            / u64::from(video_width)) as u32
            + title_height,
    );
    if let Some(work) = monitor_work_area(window) {
        let max_width = (work.right - work.left).max(1) as u32;
        let max_height = (work.bottom - work.top).max(1) as u32;
        if desired.width > max_width || desired.height > max_height {
            let content_height = max_height.saturating_sub(title_height).max(1);
            desired = PhysicalSize::new(
                ((u64::from(content_height) * u64::from(video_width) + u64::from(video_height) / 2)
                    / u64::from(video_height)) as u32,
                max_height,
            );
        }
    }
    *pending_aspect_size = Some(desired);
    let _ = window.request_inner_size(desired);
}

pub(in crate::application::viewer) fn resize_window_one_to_one(
    window: &Window,
    (video_width, video_height): (u32, u32),
    pending_aspect_size: &mut Option<PhysicalSize<u32>>,
) {
    let title_height = title_bar_height_pixels(window);
    let size = window.inner_size();
    let (max_width, max_height) = match (monitor_work_area(window), window.outer_position()) {
        (Some(work), Ok(origin)) => {
            let outer = window.outer_size();
            // Only grow to the right/bottom from the existing outer origin.
            // Reserve non-client pixels as well; never move the window to fit.
            (
                (work.right.saturating_sub(origin.x).max(1) as u32)
                    .saturating_sub(outer.width.saturating_sub(size.width))
                    .max(1),
                (work.bottom.saturating_sub(origin.y).max(1) as u32)
                    .saturating_sub(outer.height.saturating_sub(size.height))
                    .max(1),
            )
        }
        _ => (size.width.max(1), size.height.max(1)),
    };
    let max_content_height = max_height.saturating_sub(title_height).max(1);
    let scale = 1.0_f64
        .min(max_width as f64 / video_width.max(1) as f64)
        .min(max_content_height as f64 / video_height.max(1) as f64);
    let target = PhysicalSize::new(
        (video_width as f64 * scale).round().max(1.0) as u32,
        (video_height as f64 * scale).round().max(1.0) as u32 + title_height,
    );
    *pending_aspect_size = Some(target);
    let _ = window.request_inner_size(target);
}

pub(in crate::application::viewer) fn borderless_resize(
    ui: &mut egui::Ui,
    window: &Window,
    mut manual: Option<(&mut WindowResizeState, Option<(u32, u32)>)>,
) -> Option<ResizeDirection> {
    let ctx = ui.ctx().clone();
    let mut requested = None;
    crate::ui::chrome::resize_regions(ui, window, |response, direction| {
        if let Some((state, aspect)) = manual.as_mut() {
            update_nonmodal_window_resize(&ctx, window, response, direction, state, *aspect);
        } else if response.drag_started() {
            requested = Some(direction);
        }
    });
    requested
}
