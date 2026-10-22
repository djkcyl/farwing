//! Viewer-only stream settings UI. Protocol decisions stay in stream_control.
use crate::i18n::t;
use egui::{Align, FontId, RichText, Sense, Stroke, vec2};

use crate::features::stream_control::{
    MouseMode, StreamControlHandle, StreamControlSettings, StreamQuality,
};
use crate::media::FrameRateChoice;

mod display_menu;
pub(super) mod topology_menu;

pub(super) fn tab_display_menu(
    ui: &mut egui::Ui,
    handle: &StreamControlHandle,
    screen_id: i32,
    local_size: Option<(u32, u32)>,
) -> Option<String> {
    display_menu::context_menu(ui, handle, screen_id, local_size)
}

const WIDTH: f32 = 280.0;
const ROW_HEIGHT: f32 = crate::ui::theme::MENU_HEIGHT;
const ROW_GAP: f32 = 2.0;
const SECTION_GAP: f32 = 4.0;
use crate::ui::theme::{ACCENT, HOVER, LINE, MUTED, TEXT};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Quality,
    Codec,
    WeakNetwork,
    Custom,
    Advanced,
    Audio,
    Microphone,
    Mouse,
    DisplayAspect,
    Display,
}

impl Page {
    fn title(self) -> &'static str {
        let t = &t().viewer;
        match self {
            Self::Quality => t.quality,
            Self::Codec => t.page_codec,
            Self::WeakNetwork => t.page_weak_network,
            Self::Custom => t.custom_bitrate,
            Self::Advanced => t.advanced_settings,
            Self::Audio => t.audio,
            Self::Microphone => t.microphone,
            Self::Mouse => t.mouse_mode,
            Self::DisplayAspect => t.local_aspect,
            Self::Display => t.display_settings,
        }
    }
}

#[derive(Default)]
pub(super) struct StreamControlUi {
    pub(super) open: bool,
    page: Page,
    settings: Option<StreamControlSettings>,
    dirty: bool,
    display: display_menu::DisplayMenu,
    pub(super) local_error: Option<String>,
    format_confirm: Option<(i32, StreamControlSettings)>,
    /// A running "Get remote clipboard": the number of formats or the error.
    clipboard_pull: Option<std::sync::Arc<std::sync::Mutex<Option<Result<usize, String>>>>>,
}

pub(super) struct LocalViewSettings {
    pub performance_mode: super::PerformancePanelMode,
    pub performance_always_visible: bool,
    pub intercept_shortcuts: bool,
    pub system_command: Option<crate::features::remote_input::SystemCommand>,
}

enum Action {
    Apply,
    Color(bool),
    Hdr(bool),
}

#[derive(Clone, Copy)]
enum Icon {
    Back,
    Close,
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, hint: &str) -> egui::Response {
    if matches!(icon, Icon::Close) {
        return crate::ui::controls::close_button(ui, hint, crate::ui::controls::COMPACT_HEIGHT);
    }
    let (rect, response) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    if response.hovered() && ui.is_enabled() {
        ui.painter().rect_filled(rect, 4.0, HOVER);
    }
    let center = rect.center();
    let stroke = Stroke::new(1.3, if response.hovered() { TEXT } else { MUTED });
    let point = |x, y| center + vec2(x, y);
    match icon {
        Icon::Back => {
            ui.painter()
                .line_segment([point(2.5, -5.0), point(-2.5, 0.0)], stroke);
            ui.painter()
                .line_segment([point(-2.5, 0.0), point(2.5, 5.0)], stroke);
        }
        Icon::Close => crate::ui::controls::paint_close(ui.painter(), rect, stroke.color),
    }
    response.on_hover_text(hint)
}

fn separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0, LINE),
    );
}

fn section_separator(ui: &mut egui::Ui) {
    ui.add_space(SECTION_GAP);
    separator(ui);
    ui.add_space(SECTION_GAP);
}

use crate::ui::controls::menu_row;

/// Whether the host granted `cap` to this participant (until it says,
/// everything is offered).
fn granted(handle: &StreamControlHandle, cap: &str) -> bool {
    handle.granted().is_none_or(|grants| grants.iter().any(|c| c == cap))
}

/// Why something is unavailable: its permission was not granted.
fn missing(cap: &str) -> String {
    (t().viewer.permission_missing)(crate::application::app::friends::cap_label(cap))
}

fn switch_row(ui: &mut egui::Ui, label: &str, value: &mut bool) -> egui::Response {
    let enabled = ui.is_enabled();
    let (row, mut response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if enabled && response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *value, label)
    });
    if enabled && response.hovered() {
        ui.painter().rect_filled(row, 4.0, HOVER);
    }
    ui.painter().text(
        row.left_center() + vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(crate::ui::theme::COMPACT_TEXT),
        if enabled {
            TEXT
        } else {
            crate::ui::theme::DISABLED
        },
    );
    let rect = egui::Rect::from_center_size(row.right_center() - vec2(25.0, 0.0), vec2(34.0, 20.0));
    ui.painter().rect_filled(
        rect,
        10.0,
        if *value && enabled {
            ACCENT
        } else {
            crate::ui::theme::LINE
        },
    );
    let center = egui::pos2(
        if *value {
            rect.right() - 10.0
        } else {
            rect.left() + 10.0
        },
        rect.center().y,
    );
    ui.painter()
        .circle_filled(center, 7.0, crate::ui::theme::TEXT);
    response
}

fn speaker_button(ui: &mut egui::Ui, volume: u8, muted: &mut bool) {
    let (rect, mut response) = ui.allocate_exact_size(vec2(ROW_HEIGHT, ROW_HEIGHT), Sense::click());
    if response.clicked() {
        *muted = !*muted;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            ui.is_enabled(),
            *muted,
            t().viewer.mute,
        )
    });
    if response.hovered() || response.has_focus() {
        ui.painter().rect_filled(rect, 4.0, HOVER);
    }
    let stage = if *muted || volume == 0 {
        0
    } else if volume <= 33 {
        1
    } else if volume <= 66 {
        2
    } else {
        3
    };
    let color = if stage == 0 { MUTED } else { TEXT };
    let stroke = Stroke::new(1.3, color);
    let center = rect.center();
    ui.painter().add(egui::Shape::closed_line(
        [
            (-9.0, -3.0),
            (-6.0, -3.0),
            (-2.0, -7.0),
            (-2.0, 7.0),
            (-6.0, 3.0),
            (-9.0, 3.0),
        ]
        .into_iter()
        .map(|(x, y)| center + vec2(x, y))
        .collect(),
        stroke,
    ));
    if stage == 0 {
        ui.painter()
            .line_segment([center + vec2(3.0, -3.0), center + vec2(9.0, 3.0)], stroke);
        ui.painter()
            .line_segment([center + vec2(3.0, 3.0), center + vec2(9.0, -3.0)], stroke);
    } else {
        for arc in 0..stage {
            let radius = 6.0 + arc as f32 * 3.0;
            let points = (0..=10)
                .map(|step| {
                    let angle = -0.8 + step as f32 * 0.16;
                    center + vec2(-4.0 + radius * angle.cos(), radius * angle.sin())
                })
                .collect();
            ui.painter().add(egui::Shape::line(points, stroke));
        }
    }
    response.on_hover_text(if *muted {
        t().viewer.unmute
    } else {
        t().viewer.mute
    });
}

#[derive(Clone, Copy)]
struct VolumeMeterMotion {
    levels: [f32; 2],
    time: f64,
}

fn volume_bar(
    ui: &mut egui::Ui,
    volume: &mut u8,
    muted: &mut bool,
    audio: &crate::media::audio::AudioPlayback,
) -> (egui::Response, bool) {
    ui.horizontal(|ui| {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
        speaker_button(ui, *volume, muted);
        ui.spacing_mut().slider_width =
            (ui.available_width() - ROW_HEIGHT - ui.spacing().item_spacing.x - 4.0).max(40.0);
        ui.spacing_mut().interact_size.y = ROW_HEIGHT;
        // Reserve a short extension for amplification; 100% remains a clear stop.
        let gain_span = 25.0_f32;
        let gain_range = f32::from(crate::media::audio::MAX_VOLUME - 100);
        let mut position = if *volume <= 100 {
            f32::from(*volume)
        } else {
            100.0 + f32::from(*volume - 100) / gain_range * gain_span
        };
        let response = ui.add(
            egui::Slider::new(&mut position, 0.0..=100.0 + gain_span)
                .show_value(false)
                .step_by(1.0)
                .smart_aim(false)
                .handle_shape(egui::style::HandleShape::Rect { aspect_ratio: 0.0 }),
        );
        // Slider allocates Sense::drag(), which never reports double clicks.
        let changed = response.changed();
        let mut response = response.interact(Sense::click_and_drag());
        let snap_id = response.id.with("volume-unity-snap");
        let pointer_action = response.is_pointer_button_down_on()
            || response.dragged()
            || response.drag_stopped()
            || response.clicked();
        if pointer_action && let Some(pointer) = response.interact_pointer_pos() {
            let unity_x =
                response.rect.left() + response.rect.width() * 100.0 / (100.0 + gain_span);
            let was_snapped = ui
                .ctx()
                .data(|data| data.get_temp::<bool>(snap_id).unwrap_or(false));
            let snapped = (pointer.x - unity_x).abs() <= if was_snapped { 8.0 } else { 4.0 };
            ui.ctx().data_mut(|data| data.insert_temp(snap_id, snapped));
            if snapped {
                position = 100.0;
            }
        } else {
            ui.ctx().data_mut(|data| data.remove::<bool>(snap_id));
        }
        if response.double_clicked() {
            position = 100.0;
            ui.ctx().data_mut(|data| data.remove::<bool>(snap_id));
        }
        if changed || pointer_action || response.double_clicked() {
            let next = if position <= 100.0 {
                position.round() as u8
            } else {
                (100.0 + (position - 100.0) / gain_span * gain_range).round() as u8
            }
            .min(crate::media::audio::MAX_VOLUME);
            if *volume != next {
                *volume = next;
                response.mark_changed();
            }
        }
        let input = audio.input_levels().into_iter().fold(0.0_f32, f32::max);
        let output = if *muted || *volume == 0 {
            0.0
        } else {
            audio.output_levels().into_iter().fold(0.0_f32, f32::max)
        };
        let target = [input, output].map(|amplitude| {
            if amplitude > 0.0 {
                ((20.0 * amplitude.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
            } else {
                0.0
            }
        });
        let now = ui.input(|input| input.time);
        let levels = ui.ctx().data_mut(|data| {
            let id = response.id.with("volume-meter-motion");
            let mut motion = data
                .get_temp::<VolumeMeterMotion>(id)
                .unwrap_or(VolumeMeterMotion {
                    levels: target,
                    time: now,
                });
            let elapsed = now - motion.time;
            if !(0.0..=0.5).contains(&elapsed) {
                motion.levels = target;
            } else {
                for (shown, target) in motion.levels.iter_mut().zip(target) {
                    // Display-only ballistics. Gain and the audio callback do
                    // not wait for this visual attack/release.
                    let tau = if target > *shown { 0.12 } else { 0.45 };
                    *shown += (target - *shown) * (1.0 - (-elapsed / tau).exp()) as f32;
                    if target == 0.0 && *shown < 0.002 {
                        *shown = 0.0;
                    }
                }
            }
            if *muted || *volume == 0 {
                motion.levels[1] = 0.0;
            }
            motion.time = now;
            data.insert_temp(id, motion);
            motion.levels
        });
        let rect = response.rect;
        // Keep the full row as the hit target, but draw only a slim rail.
        ui.painter().rect_filled(rect, 0.0, crate::ui::theme::BG);
        let track = egui::Rect::from_center_size(rect.center(), vec2(rect.width(), 6.0));
        ui.painter()
            .rect_filled(track, 3.0, crate::ui::theme::SIDEBAR);
        let normal_width = track.width() * 100.0 / (100.0 + gain_span);
        let unity_x = track.left() + normal_width;
        let gain_track = egui::Rect::from_min_max(egui::pos2(unity_x, track.top()), track.max);
        ui.painter().rect_filled(
            gain_track,
            3.0,
            crate::ui::theme::AMBER.gamma_multiply(0.18),
        );
        if *volume > 100 && !*muted {
            let boost_width = gain_track.width() * f32::from(*volume - 100) / gain_range;
            ui.painter().rect_filled(
                egui::Rect::from_min_size(gain_track.min, vec2(boost_width, gain_track.height())),
                2.0,
                crate::ui::theme::AMBER,
            );
        }
        for (level, color) in [
            (levels[0], crate::ui::theme::MUTED),
            (levels[1], crate::ui::theme::GREEN),
        ] {
            if level > 0.0 {
                let fill = egui::Rect::from_min_max(
                    track.min,
                    egui::pos2(track.left() + normal_width * level, track.bottom()),
                );
                ui.painter()
                    .with_clip_rect(fill.intersect(ui.clip_rect()))
                    .rect_filled(track, 3.0, color);
            }
        }
        ui.painter().line_segment(
            [
                egui::pos2(unity_x, track.top() - 2.0),
                egui::pos2(unity_x, track.bottom() + 2.0),
            ],
            Stroke::new(1.0, MUTED),
        );
        let handle_x = (rect.left() + rect.width() * position / (100.0 + gain_span))
            .clamp(rect.left() + 2.0, rect.right() - 2.0);
        ui.painter().rect_filled(
            egui::Rect::from_center_size(egui::pos2(handle_x, rect.center().y), vec2(2.5, 14.0)),
            1.0,
            if *volume > 100 {
                crate::ui::theme::AMBER
            } else {
                TEXT
            },
        );
        if response.hovered() || response.dragged() || response.has_focus() {
            ui.painter().rect_stroke(
                track,
                3.0,
                Stroke::new(1.0, ACCENT),
                egui::StrokeKind::Inside,
            );
        }
        ui.add_space(4.0);
        let choose_output =
            crate::ui::controls::audio_output_button(ui, t().viewer.audio_settings, ROW_HEIGHT)
                .clicked();
        let hint = if *volume > 100 {
            (t().viewer.volume_gain)(*volume, 20.0 * (f32::from(*volume) / 100.0).log10())
        } else {
            (t().viewer.volume)(*volume)
        };
        (response.on_hover_text(hint), choose_output)
    })
    .inner
}

fn bitrate_editor(ui: &mut egui::Ui, value: &mut u32, multi_screen: bool, limit: u32) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(if multi_screen {
            t().viewer.bitrate_per_screen
        } else {
            t().viewer.video_bitrate
        });
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(format!("{value}/{limit} Mbps"));
        });
    });
    ui.add_space(6.0);
    changed |= ui
        .scope(|ui| {
            ui.spacing_mut().slider_width = ui.available_width();
            ui.spacing_mut().slider_rail_height = 3.0;
            ui.spacing_mut().interact_size.y = 18.0;
            let choices = crate::features::stream_control::custom_bitrate_choices(limit);
            let mut index = choices.iter().rposition(|n| *n <= *value).unwrap_or(0);
            let response = ui.add(
                egui::Slider::new(&mut index, 0..=choices.len() - 1)
                    .show_value(false)
                    .trailing_fill(true)
                    .handle_shape(egui::style::HandleShape::Circle),
            );
            if response.changed() {
                *value = choices[index];
            }
            response.changed()
        })
        .inner;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("1")
                .size(crate::ui::theme::MICRO)
                .color(MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(limit.to_string())
                    .size(crate::ui::theme::MICRO)
                    .color(MUTED),
            );
        });
    });
    changed
}

pub(super) fn menu_style(ui: &mut egui::Ui) {
    let style = ui.style_mut();
    style.override_text_style = Some(egui::TextStyle::Body);
    style.spacing.item_spacing = vec2(6.0, ROW_GAP);
    crate::ui::controls::configure(style, ROW_HEIGHT);
    style.interaction.selectable_labels = false;
}

pub(super) fn show_stream_control_window(
    ctx: &egui::Context,
    handle: &StreamControlHandle,
    state: &mut StreamControlUi,
    view: &mut LocalViewSettings,
    screen_id: i32,
    local_size: Option<(u32, u32)>,
    anchor_offset: egui::Vec2,
) {
    topology_menu::show(ctx, handle, local_size);
    topology_menu::show_error(ctx, handle);
    let mic = handle.microphone().snapshot();
    crate::ui::controls::observe_notice(
        ctx,
        "microphone-error",
        t().viewer.microphone,
        crate::ui::controls::DialogIcon::Error,
        mic.error.as_deref(),
    );
    let audio = handle.audio();
    crate::ui::controls::progress_notice(
        ctx,
        "audio-device-query",
        t().viewer.audio_devices,
        (state.open && state.page == Page::Audio && !audio.output_devices().loaded)
            .then_some(t().viewer.reading_output_devices),
    );
    let audio_error = audio.snapshot().error;
    if crate::ui::controls::observe_notice_action(
        ctx,
        "audio-output-error",
        t().viewer.audio_output_problem,
        crate::ui::controls::DialogIcon::Error,
        audio_error.as_deref(),
        t().common.retry,
    ) == Some(true)
    {
        crate::ui::controls::clear_notice(ctx, "audio-output-error");
        audio.retry();
        let _ = audio.refresh_output_devices();
    }
    if crate::ui::controls::observe_notice_action(
        ctx,
        "audio-device-query-error",
        t().viewer.audio_device_check_failed,
        crate::ui::controls::DialogIcon::Error,
        audio
            .output_devices()
            .error
            .as_deref()
            .filter(|_| state.open && state.page == Page::Audio),
        t().viewer.check_again,
    ) == Some(true)
    {
        crate::ui::controls::clear_notice(ctx, "audio-device-query-error");
        state.local_error = audio
            .refresh_output_devices()
            .err()
            .map(|error| error.to_string());
    }
    let snapshot = handle.snapshot();
    crate::ui::controls::observe_notice(
        ctx,
        "remote-input-error",
        t().viewer.control_stopped,
        crate::ui::controls::DialogIcon::Error,
        snapshot.mouse_error.as_deref(),
    );
    state.display.select_screen(screen_id);
    crate::ui::controls::progress_notice(
        ctx,
        "network-switch-progress",
        t().viewer.switch_route,
        (state.open && snapshot.network.pending).then_some(t().viewer.switching_route),
    );
    crate::ui::controls::observe_notice(
        ctx,
        "network-switch-result",
        t().viewer.route,
        crate::ui::controls::DialogIcon::Info,
        snapshot.network.notice.as_deref().filter(|_| state.open),
    );

    crate::ui::controls::observe_notice(
        ctx,
        "remote-session-notice",
        t().viewer.remote_notice,
        crate::ui::controls::DialogIcon::Info,
        snapshot.remote_notice.as_deref(),
    );
    let multi_screen = snapshot.screens.len() > 1;
    if !state.open {
        state.page = Page::Quality;
        state.settings = None;
        state.dirty = false;
        state.local_error = None;
        state.display.reset();
        state.format_confirm = None;
        return;
    }
    if !state.dirty && state.local_error.is_none() {
        state.settings = Some(snapshot.settings);
    }
    let mut settings = state.settings.unwrap_or(snapshot.settings);
    let mut open = state.open;
    let mut action = None;
    let mut back = false;
    let mut close = false;
    if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        open = false;
    }
    let menu_width = if state.page == Page::Microphone {
        crate::ui::theme::AUDIO_MENU_WIDTH
    } else {
        WIDTH
    };
    egui::Window::new(t().viewer.stream_quality)
        .id(egui::Id::new("runtime-stream-settings-window"))
        .open(&mut open)
        .collapsible(false)
        .auto_sized()
        .title_bar(false)
        .anchor(egui::Align2::RIGHT_TOP, anchor_offset)
        .default_width(menu_width + 24.0)
        .frame(
            egui::Frame::new()
                .fill(crate::ui::theme::BG)
                .stroke(Stroke::new(1.0, LINE))
                .corner_radius(crate::ui::theme::PANEL_RADIUS)
                .inner_margin(egui::Margin::symmetric(12, 6)),
        )
        .show(ctx, |ui| {
            menu_style(ui);
            ui.set_width(menu_width);
            ui.horizontal(|ui| {
                if state.page != Page::Quality {
                    back = icon_button(ui, Icon::Back, t().common.back).clicked();
                }
                let title = if state.page == Page::Display {
                    t().viewer.display_settings.to_owned()
                } else if multi_screen && matches!(state.page, Page::Quality | Page::Custom) {
                    (t().viewer.all_screens)(state.page.title())
                } else {
                    state.page.title().to_owned()
                };
                ui.label(
                    RichText::new(title)
                        .size(crate::ui::theme::COMPACT_TEXT)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    close = icon_button(ui, Icon::Close, t().common.close).clicked();
                });
            });
            ui.add_space(SECTION_GAP);
            ui.scope(|ui| {
                ui.set_width(menu_width);
                match state.page {
                    Page::Display => {
                        egui::ScrollArea::vertical()
                            .max_height((ctx.content_rect().height() - 150.0).max(140.0))
                            .show(ui, |ui| {
                                state
                                    .display
                                    .draw(ui, handle, &snapshot, screen_id, local_size);
                            });
                        // Its confirmation takes over from the menu.
                        close |= std::mem::take(&mut state.display.topology_requested);
                    }
                    Page::Quality => {
                        egui::ScrollArea::vertical()
                            .id_salt("stream-settings-root")
                            .max_height((ctx.content_rect().height() - 150.0).max(140.0))
                            .show(ui, |ui| {
                                let preferences = handle.device_preferences();
                                let codec = preferences.codec;
                                // Several participants: only the one holding control
                                // chooses its picture; the others share one stream.
                                let locked = handle.participants().shared_view();
                                if locked {
                                    ui.label(
                                        RichText::new((t().viewer.shared_view)(
                                            crate::features::host::shared_view::FPS,
                                        ))
                                        .size(crate::ui::theme::SMALL)
                                        .strong(),
                                    );
                                    section_separator(ui);
                                }
                                if menu_row(ui, t().viewer.page_codec, codec.map_or(t().viewer.follow_global, |value| value.label()), None, !locked, true).clicked() {
                                    state.page = Page::Codec;
                                }
                                if menu_row(ui, t().viewer.page_weak_network, preferences.weak_network.label(), None, !locked, true)
                                    .clicked()
                                {
                                    state.page = Page::WeakNetwork;
                                }
                                let mut cinema = preferences.cinema || locked;
                                if ui
                                    .add_enabled_ui(!locked, |ui| {
                                        switch_row(ui, t().viewer.cinema_mode, &mut cinema)
                                    })
                                    .inner
                                    .changed()
                                {
                                    if let Err(error) = handle.set_playback(preferences.weak_network, cinema) {
                                        state.local_error = Some(error.to_string());
                                    }
                                }
                                section_separator(ui);
                                for (quality, label, detail) in [
                                    (
                                        StreamQuality::Auto,
                                        snapshot.auto_quality_label.as_str(),
                                        String::new(),
                                    ),
                                    (StreamQuality::Original, t().viewer.quality_original, "30M".into()),
                                    (StreamQuality::High, t().viewer.quality_very_high, "14M".into()),
                                    (StreamQuality::Clear, t().viewer.quality_high, "8M".into()),
                                    (
                                        StreamQuality::Custom,
                                        t().viewer.custom_bitrate,
                                        format!(
                                            "{} Mbps",
                                            settings
                                                .custom_bitrate_mbps
                                                .min(snapshot.custom_bitrate_limit)
                                        ),
                                    ),
                                ] {
                                    let more = matches!(quality, StreamQuality::Custom);
                                    let enabled = snapshot.ready
                                        && !locked
                                        && (!more || snapshot.custom_bitrate_supported);
                                    let response = menu_row(
                                        ui,
                                        label,
                                        &detail,
                                        Some(settings.quality == quality),
                                        enabled,
                                        more,
                                    );
                                    if response.clicked() {
                                        if more {
                                            state.page = Page::Custom;
                                            settings.quality = StreamQuality::Custom;
                                            settings.custom_bitrate_mbps = settings
                                                .custom_bitrate_mbps
                                                .min(snapshot.custom_bitrate_limit);
                                            state.dirty = settings != snapshot.settings;
                                        }
                                        if !more
                                            && (settings.quality != quality
                                                || state.local_error.is_some()
                                                || snapshot.last_error.is_some())
                                        {
                                            settings.quality = quality;
                                            action = Some(Action::Apply);
                                        }
                                    }
                                    if more && !snapshot.custom_bitrate_supported {
                                        response.on_hover_text(t().viewer.not_supported_by_remote);
                                    }
                                }
                                ui.add_space(crate::ui::theme::MENU_GROUP_GAP);
                                // What this participant was not granted is shown, disabled, with why.
                                let display_granted = granted(handle, farwing_protocol::caps::DISPLAY);
                                let display = menu_row(
                                    ui,
                                    t().viewer.resolution_and_dpi,
                                    t().viewer.this_screen,
                                    None,
                                    snapshot.display_settings_supported && display_granted,
                                    true,
                                );
                                let display = if display_granted {
                                    display
                                } else {
                                    display.on_disabled_hover_text(missing(farwing_protocol::caps::DISPLAY))
                                };
                                if display.clicked() {
                                    state.page = Page::Display;
                                }
                                ui.add_space(crate::ui::theme::MENU_GROUP_GAP);
                                ui.horizontal(|ui| {
                                    let label = ui.label(
                                        RichText::new(t().viewer.frame_rate_limit)
                                            .size(crate::ui::theme::TINY)
                                            .color(MUTED),
                                    );
                                    if let Some(notice) = snapshot.last_notice.as_deref() {
                                        label.on_hover_text(notice);
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new("FPS")
                                                    .size(crate::ui::theme::MICRO)
                                                    .color(MUTED),
                                            );
                                        },
                                    );
                                });
                                ui.add_space(ROW_GAP);
                                let choices = FrameRateChoice::available(snapshot.local_display)
                                    .into_iter()
                                    .filter(|choice| *choice != FrameRateChoice::Auto)
                                    .collect::<Vec<_>>();
                                let width = (WIDTH
                                    - 6.0 * (choices.len().saturating_sub(1)) as f32)
                                    / choices.len().max(1) as f32;
                                ui.add_enabled_ui(snapshot.ready && !locked, |ui| {
                                    ui.horizontal(|ui| {
                                        for choice in choices {
                                            if ui
                                                .add_sized(
                                                    [width, ROW_HEIGHT],
                                                    egui::Button::new(
                                                        choice
                                                            .value(snapshot.local_display)
                                                            .to_string(),
                                                    )
                                                    .selected(
                                                        settings
                                                            .frame_rate
                                                            .value(snapshot.local_display)
                                                            == choice.value(snapshot.local_display),
                                                    ),
                                                )
                                                .clicked()
                                                && settings.frame_rate != choice
                                            {
                                                settings.frame_rate = choice;
                                                action = Some(Action::Apply);
                                            }
                                        }
                                    });
                                });
                                if snapshot.true_color_supported {
                                    ui.add_space(crate::ui::theme::MENU_GROUP_GAP);
                                    ui.label(
                                        RichText::new(t().viewer.chroma_subsampling)
                                            .size(crate::ui::theme::TINY)
                                            .color(MUTED),
                                    );
                                    ui.add_space(ROW_GAP);
                                    ui.add_enabled_ui(
                                        snapshot.ready && snapshot.pending_sequence.is_none() && !locked,
                                        |ui| {
                                            ui.horizontal(|ui| {
                                                for (enabled, label) in
                                                    [(true, "YUV 4:4:4"), (false, "YUV 4:2:0")]
                                                {
                                                    if ui
                                                        .add_sized(
                                                            [(WIDTH - 6.0) / 2.0, ROW_HEIGHT],
                                                            egui::Button::new(label).selected(
                                                                settings.true_color == enabled,
                                                            ),
                                                        )
                                                        .clicked()
                                                        && settings.true_color != enabled
                                                    {
                                                        action = Some(Action::Color(enabled));
                                                    }
                                                }
                                            });
                                        },
                                    );
                                }
                                if snapshot.hdr_supported {
                                    ui.add_space(SECTION_GAP);
                                    let mut hdr = settings.hdr;
                                    let enabled = snapshot.ready
                                        && snapshot.pending_sequence.is_none()
                                        && !locked
                                        && (hdr || snapshot.hdr_unavailable.is_none());
                                    let response = ui
                                        .add_enabled_ui(enabled, |ui| {
                                            switch_row(ui, "HDR", &mut hdr)
                                        })
                                        .inner;
                                    if response.changed() {
                                        action = Some(Action::Hdr(hdr));
                                    }
                                    if let Some(reason) = snapshot.hdr_unavailable.as_deref() {
                                        response.on_hover_text(reason);
                                    }
                                }
                                section_separator(ui);
                                if menu_row(ui, t().viewer.advanced_settings, "", None, true, true).clicked() {
                                    state.page = Page::Advanced;
                                }
                                let audio = handle.audio();
                                let mut audio_settings = audio.settings();
                                let audio_status = audio.snapshot();
                                let (volume_response, choose_output) = volume_bar(
                                    ui,
                                    &mut audio_settings.volume,
                                    &mut audio_settings.muted,
                                    &audio,
                                );
                                if choose_output {
                                    state.page = Page::Audio;
                                    state.local_error = audio.refresh_output_devices().err().map(|error| error.to_string());
                                }
                                if audio_status.device.is_empty() && !audio_status.receiving {
                                    volume_response.on_hover_text(t().viewer.waiting_for_audio);
                                } else if !audio_status.device.is_empty() {
                                    volume_response.on_hover_text((t().viewer.audio_output)(&audio_status.device));
                                }
                                audio.set_settings(audio_settings);
                            });
                    }
                    Page::Codec => {
                        let codec = handle.device_preferences().codec;
                        for value in std::iter::once(None)
                            .chain(crate::media::CodecPreference::PREFERRED.into_iter().map(Some)) {
                            if menu_row(ui, value.map_or(t().viewer.follow_global, |value| value.label()), "", Some(codec == value), true, false).clicked()
                                && codec != value
                                && let Err(error) = handle.set_codec_preference(value)
                            {
                                state.local_error = Some(error.to_string());
                            }
                        }
                    }
                    Page::WeakNetwork => {
                        let preferences = handle.device_preferences();
                        for value in [
                            crate::features::viewing_settings::WeakNetwork::Bitrate,
                            crate::features::viewing_settings::WeakNetwork::FrameRate,
                        ] {
                            if menu_row(ui, value.label(), "", Some(preferences.weak_network == value), true, false).clicked()
                                && preferences.weak_network != value
                            {
                                if let Err(error) = handle.set_playback(value, preferences.cinema) {
                                    state.local_error = Some(error.to_string());
                                }
                            }
                        }
                    }
                    Page::Audio => {
                        crate::ui::controls::remote_audio_quality(ui,handle);
                        section_separator(ui);
                        let audio = handle.audio();
                        let selected = audio.selected_output();
                        let outputs = audio.output_devices();
                        ctx.request_repaint_after(std::time::Duration::from_millis(250));
                        if menu_row(ui, t().viewer.follow_system_default, "", Some(selected.is_none()), true, false).clicked() {
                            audio.set_output_device(None);
                        }
                        section_separator(ui);
                        egui::ScrollArea::vertical()
                            .id_salt("audio-output-devices")
                            .max_height((ctx.content_rect().height() - 250.0).clamp(100.0, 360.0))
                            .show(ui, |ui| {
                                for output in &outputs.devices {
                                    if menu_row(ui, &output.name, "", Some(selected.as_ref() == Some(&output.id)), true, false)
                                        .on_hover_text(&output.name).clicked() {
                                        audio.set_output_device(Some(output.id.clone()));
                                    }
                                }
                                if outputs.loaded && outputs.devices.is_empty() && outputs.error.is_none() {
                                    ui.label(RichText::new(t().viewer.no_output_devices).color(MUTED));
                                }
crate::ui::controls::observe_notice(ui.ctx(), "audio-output-disconnected", t().viewer.audio_device_disconnected, crate::ui::controls::DialogIcon::Warning, (outputs.loaded && selected.as_ref().is_some_and(|id| !outputs.devices.iter().any(|device| &device.id == id))).then_some(t().viewer.selected_device_disconnected));
                            });
                    }
                    Page::Advanced => {
                        egui::ScrollArea::vertical()
                            .id_salt("stream-settings-advanced")
                            .max_height((ctx.content_rect().height() - 150.0).max(140.0))
                            .show(ui, |ui| {
                                let aspect = handle.device_preferences().display_aspect;
                                let label = match aspect {
                                    crate::media::geometry::DisplayAspect::Video => t().viewer.aspect_keep_video,
                                    crate::media::geometry::DisplayAspect::Display => t().viewer.aspect_follow_display,
                                    crate::media::geometry::DisplayAspect::Wide => "16:9",
                                    crate::media::geometry::DisplayAspect::WideTen => "16:10",
                                    crate::media::geometry::DisplayAspect::Standard => "4:3",
                                    crate::media::geometry::DisplayAspect::UltraWide => "21:9",
                                };
                                if menu_row(ui, t().viewer.local_aspect, label, None, true, true).clicked() {
                                    state.page = Page::DisplayAspect;
                                }
                                if menu_row(ui, t().viewer.audio_only_mode, "", None, true, false).clicked() {
                                    handle.request_audio_only(true);
                                    state.open = false;
                                }
                                section_separator(ui);
                                let mut relay = snapshot.network.relay_enabled;
                                let response = ui
                                    .add_enabled_ui(snapshot.network.available, |ui| {
                                        switch_row(ui, t().viewer.fast_relay_beta, &mut relay)
                                    })
                                    .inner;
                                if response.changed() {
                                    state.local_error = handle
                                        .set_relay_enabled(relay)
                                        .err()
                                        .map(|e| e.to_string());
                                }
                                if let Some(reason) = snapshot.network.unavailable_reason {
                                    response.on_hover_text(reason);
                                }
                                switch_row(
                                    ui,
                                    t().viewer.performance_always_visible,
                                    &mut view.performance_always_visible,
                                );
                                ui.horizontal(|ui| {
                                    for (mode, label) in [
                                        (super::PerformancePanelMode::Compact, t().viewer.performance_compact),
                                        (super::PerformancePanelMode::Detailed, t().viewer.performance_detailed),
                                    ] {
                                        if ui.add_sized(
                                            [(WIDTH - 6.0) / 2.0, ROW_HEIGHT],
                                            egui::Button::new(label).selected(view.performance_mode == mode),
                                        ).clicked() {
                                            view.performance_mode = mode;
                                        }
                                    }
                                });
                                let mode_label = match snapshot.mouse_preference {
                                    MouseMode::Smart | MouseMode::View => t().viewer.mouse_smart,
                                    MouseMode::Remote => t().viewer.mouse_remote,
                                    MouseMode::Local => t().viewer.mouse_local,
                                };
                                if menu_row(ui, t().viewer.mouse_mode, mode_label, None, true, true).clicked()
                                {
                                    state.page = Page::Mouse;
                                }
                                switch_row(ui, t().viewer.intercept_shortcuts, &mut view.intercept_shortcuts);
                                let clipboard = handle.clipboard();
                                let input=handle.microphone().selected_input();
                                let devices=handle.microphone().input_devices();
                                let name=input.as_ref().map(|id|devices.devices.iter().find(|d|&d.id==id).map(|d|d.name.as_str()).unwrap_or(if devices.error.is_some() {t().viewer.selected_microphone}else{t().viewer.selected_device_disconnected})).unwrap_or(t().viewer.follow_system_default);
                                if menu_row(ui,t().viewer.microphone,name,None,true,true)
                                    .on_hover_text(name).clicked() {
                                    state.page=Page::Microphone;
                                    state.local_error=handle.microphone().refresh_input_devices().err().map(|e|e.to_string());
                                }
                                if mic.enabled {
                                    let label=if mic.pending {t().viewer.mic_waiting_remote}else if mic.capturing {t().viewer.mic_sending}else if mic.remote_active {t().viewer.mic_opening}else{t().viewer.mic_waiting_app};
                                    ui.label(RichText::new(label).color(MUTED));
                                    if mic.capturing {
                                        ui.add(egui::ProgressBar::new(mic.level.clamp(0.0,1.0)).text(&mic.device));
                                        ctx.request_repaint_after(std::time::Duration::from_millis(100));
                                    }
                                    if mic.error.is_some() && menu_row(ui,t().viewer.retry_microphone,"",None,true,false).clicked() {handle.microphone().retry();}
                                }
                                let clip = clipboard.snapshot();
                                let clipboard_granted = granted(handle, farwing_protocol::caps::CLIPBOARD);
                                let mut enabled = clip.enabled && clipboard_granted;
                                let sync = ui
                                    .add_enabled_ui(clipboard_granted, |ui| switch_row(ui, t().viewer.clipboard_sync, &mut enabled))
                                    .inner;
                                let sync = if clipboard_granted {
                                    sync
                                } else {
                                    sync.on_disabled_hover_text(missing(farwing_protocol::caps::CLIPBOARD))
                                };
                                if sync.changed() {
                                    if let Err(error) = handle.set_clipboard_enabled(enabled) {
                                        state.local_error = Some(error.to_string());
                                    }
                                }
                                if enabled {
                                    let mut files = clip.files;
                                    if switch_row(ui, t().viewer.file_copy, &mut files).changed() {
                                        handle.set_clipboard_files(files);
                                    }
                                    crate::ui::controls::progress_notice(ui.ctx(),"clipboard-wait",t().viewer.clipboard_sync,(!clip.active && snapshot.mouse_mode != MouseMode::View).then_some(t().viewer.clipboard_waiting));
crate::ui::controls::observe_notice(ui.ctx(), "clipboard-error", t().viewer.clipboard_sync_failed, crate::ui::controls::DialogIcon::Error, clip.error.as_deref());
                                }
                                // Manual copy of the host clipboard: also for participants
                                // without automatic sync (view mode, not controlling).
                                let pulling = state.clipboard_pull.is_some();
                                let pull = menu_row(ui, t().viewer.pull_clipboard, if pulling { t().viewer.pulling } else { "" }, None, clipboard_granted && clip.active && !pulling, false);
                                let pull = if clipboard_granted {
                                    pull
                                } else {
                                    pull.on_disabled_hover_text(missing(farwing_protocol::caps::CLIPBOARD))
                                };
                                if pull.clicked() {
                                    let result = std::sync::Arc::new(std::sync::Mutex::new(None));
                                    state.clipboard_pull = Some(result.clone());
                                    let clipboard = clipboard.clone();
                                    let repaint = ctx.clone();
                                    std::thread::spawn(move || {
                                        let value = clipboard.pull().map_err(|e| e.to_string());
                                        *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
                                        repaint.request_repaint();
                                    });
                                }
                                let finished = state.clipboard_pull.as_ref().and_then(|r| r.lock().unwrap_or_else(|e| e.into_inner()).take());
                                if let Some(result) = finished {
                                    state.clipboard_pull = None;
                                    match result {
                                        Ok(_) => crate::ui::controls::notice(ui.ctx(), "clipboard-pull", t().viewer.pull_clipboard, crate::ui::controls::DialogIcon::Ready, t().viewer.copied_to_local_clipboard),
                                        Err(error) => crate::ui::controls::notice(ui.ctx(), "clipboard-pull", t().viewer.pull_clipboard, crate::ui::controls::DialogIcon::Error, error),
                                    }
                                } else if pulling {
                                    ctx.request_repaint_after(std::time::Duration::from_millis(200));
                                }
                                let can_send = snapshot.ready
                                    && !snapshot.mouse_pending
                                    && snapshot.mouse_mode != MouseMode::View
                                    && handle.mouse().supports_system_commands()
                                    && !handle.mouse().waiting_for_neutral();
                                section_separator(ui);
                                use crate::features::remote_input::SystemCommand;
                                for (label, command) in [
                                    (t().viewer.send_ctrl_alt_del, SystemCommand::Sas),
                                    (t().viewer.lock_remote_device, SystemCommand::Lock),
                                    (t().viewer.task_manager, SystemCommand::TaskManager),
                                    (t().viewer.show_desktop, SystemCommand::ShowDesktop),
                                    (t().viewer.task_view, SystemCommand::TaskView),
                                ] {
                                    if menu_row(ui, label, "", None, can_send, false).clicked() {
                                        view.system_command = Some(command);
                                    }
                                }
                            });
                    }
                    Page::DisplayAspect => {
                        let aspect = handle.device_preferences().display_aspect;
                        let preferred = snapshot.screens.iter()
                            .find(|screen| screen.id == screen_id)
                            .and_then(|screen| screen.display.preferred);
                        for value in crate::media::geometry::DisplayAspect::ALL {
                            let available = value != crate::media::geometry::DisplayAspect::Display
                                || preferred.is_some();
                            if menu_row(ui, value.label(), "", Some(aspect == value), available, false).clicked() {
                                handle.set_display_aspect(value);
                            }
                        }
                        if let Some((width, height)) = preferred {
                            section_separator(ui);
                            ui.add(
                                egui::Label::new(
                                    RichText::new((t().viewer.remote_display_native_size)(
                                        width, height,
                                    ))
                                    .size(crate::ui::theme::TINY)
                                    .color(MUTED),
                                )
                                .wrap(),
                            );
                        }
                    }
                    Page::Microphone => {
                        let microphone=handle.microphone();
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(t().controls.audio_quality).size(crate::ui::theme::SMALL).color(MUTED));
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new("kbps").size(crate::ui::theme::SMALL).color(MUTED));
                            });
                        });
                        let mut quality=microphone.quality();
                        if crate::ui::controls::audio_quality_segments(ui,&mut quality) {
                            if let Err(error)=microphone.set_quality(quality) {state.local_error=Some(error.to_string());}
                        }
                        if let Some(bitrate)=microphone.target_bitrate() {
                            if bitrate<quality.kbps*1000 {ui.label(RichText::new((t().viewer.mic_target_bitrate)(bitrate/1000)).color(MUTED));}
                        }
                        ui.add_space(crate::ui::theme::MENU_GROUP_GAP);
                        ui.label(RichText::new(t().viewer.input_devices).size(crate::ui::theme::SMALL).color(MUTED));
                        let selected=microphone.selected_input();
                        let inputs=microphone.input_devices();
                        ctx.request_repaint_after(std::time::Duration::from_millis(250));
                        egui::ScrollArea::vertical().id_salt("microphone-input-devices")
                            .max_height((ctx.content_rect().height()-250.0).clamp(100.0,360.0))
                            .show(ui,|ui| {
                                if menu_row(ui,t().viewer.follow_system_default,"",Some(selected.is_none()),true,false).clicked() {
                                    microphone.set_input_device(None);
                                }
                                if inputs.updated.is_none() {ui.label(RichText::new(t().viewer.reading_microphones).color(MUTED));}
                                for input in &inputs.devices {
                                    if menu_row(ui,&input.name,"",Some(selected.as_ref()==Some(&input.id)),true,false)
                                        .on_hover_text(&input.name).clicked() {
                                        microphone.set_input_device(Some(input.id.clone()));
                                    }
                                }
                                if inputs.updated.is_some() && inputs.devices.is_empty() && inputs.error.is_none() {
                                    ui.label(RichText::new(t().viewer.no_microphones).color(MUTED));
                                }
                                if inputs.error.is_none() && selected.as_ref().is_some_and(|id| inputs.updated.is_some() && !inputs.devices.iter().any(|d|&d.id==id)) {
                                    ui.label(RichText::new(t().viewer.selected_microphone_disconnected).color(MUTED));
                                }
                            });
                        crate::ui::controls::observe_notice(ui.ctx(),"microphone-device-query",t().viewer.microphone_devices,crate::ui::controls::DialogIcon::Error,inputs.error.as_deref());
                    }
                    Page::Mouse => {
                        for (mode, label) in [
                            (MouseMode::Smart, t().viewer.mouse_smart),
                            (MouseMode::Remote, t().viewer.use_remote_mouse),
                            (MouseMode::Local, t().viewer.use_local_mouse),
                        ] {
                            if menu_row(
                                ui,
                                label,
                                "",
                                Some(snapshot.mouse_preference == mode),
                                snapshot.ready
                                    && snapshot.mouse_modes_supported
                                    && !snapshot.mouse_pending,
                                false,
                            )
                            .clicked()
                            {
                                crate::ui::controls::clear_notice(ctx, "remote-input-error");
                                state.local_error = handle
                                    .set_mouse_mode(mode)
                                    .err()
                                    .map(|error| error.to_string());
                            }
                        }
                        section_separator(ui);
                        let preferences = handle.device_preferences();
                        let mut throttle = preferences.mouse_throttle;
                        let mut rate = preferences.mouse_throttle_rate;
                        let mut changed = switch_row(ui, t().viewer.mouse_throttle, &mut throttle).changed();
                        ui.add_enabled_ui(throttle, |ui| {
                            ui.horizontal(|ui| {
                                for choice in crate::features::remote_input::MouseThrottleRate::ALL {
                                    if ui.add_sized(
                                        [(WIDTH - 12.0) / 3.0, ROW_HEIGHT],
                                        egui::Button::new(choice.label()).selected(rate == choice),
                                    ).clicked() {
                                        changed |= rate != choice;
                                        rate = choice;
                                    }
                                }
                            });
                        });
                        if changed {
                            state.local_error = handle.set_mouse_throttle(throttle, rate).err().map(|e|format!("{e:#}"));
                        }
                        separator(ui);
                        ui.add_space(7.0);
                        ui.label(
                            RichText::new((t().viewer.release_shortcut)(
                                &crate::application::viewer_shortcuts::label(
                                    crate::application::viewer_shortcuts::Action::ReleaseMouse,
                                ),
                            ))
                            .size(crate::ui::theme::TINY)
                            .color(MUTED),
                        );
                    }
                    Page::Custom => {
                        ui.add_enabled_ui(snapshot.ready, |ui| {
                            state.dirty |= bitrate_editor(
                                ui,
                                &mut settings.custom_bitrate_mbps,
                                multi_screen,
                                snapshot.custom_bitrate_limit,
                            );
                            ui.add_space(16.0);
                            let can_apply = state.dirty
                                || state.local_error.is_some()
                                || snapshot.last_error.is_some();
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(if state.dirty { t().viewer.not_applied } else { "" })
                                        .size(crate::ui::theme::TINY)
                                        .color(MUTED),
                                );
                                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                    if ui
                                        .add_enabled(
                                            can_apply,
                                            egui::Button::new(t().common.apply)
                                                .fill(if can_apply { ACCENT } else { HOVER })
                                                .stroke(Stroke::NONE)
                                                .min_size(vec2(76.0, 30.0)),
                                        )
                                        .clicked()
                                    {
                                        action = Some(Action::Apply);
                                    }
                                });
                            });
                        });
                    }
                }
                if state.page != Page::Display {
                    crate::ui::controls::observe_notice(ui.ctx(), "stream-settings-error", t().viewer.settings_not_applied, crate::ui::controls::DialogIcon::Error, state.local_error.as_deref().or(snapshot.cursor_error.as_deref()).or(snapshot.last_error.as_deref()).or(snapshot.network.error.as_deref()));
                    crate::ui::controls::observe_notice(ui.ctx(), "stream-settings-save", t().viewer.settings_not_saved, crate::ui::controls::DialogIcon::Error, snapshot.persistence_error.as_deref());
                }
            });
        });
    if let Some((target, proposed)) = state.format_confirm {
        let response = egui::Modal::new(egui::Id::new("stream-format-confirmation"))
            .frame(crate::ui::controls::dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(340.0);
                let close = crate::ui::controls::dialog_header(
                    ui,
                    t().viewer.change_display_format,
                    crate::ui::controls::DialogIcon::Info,
                    true,
                );
                ui.label(format!(
                    "{} · {} · {}",
                    proposed.quality.label(),
                    if proposed.true_color {
                        "YUV 4:4:4"
                    } else {
                        "YUV 4:2:0"
                    },
                    if proposed.hdr { "HDR" } else { "SDR" }
                ));
                let (apply, cancel) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(crate::ui::controls::DialogAction::new(t().common.apply)),
                    Some(t().common.cancel),
                );
                {
                    if close || cancel {
                        state.format_confirm = None;
                    }
                    if apply {
                        state.local_error = handle
                            .apply_format(target, proposed)
                            .err()
                            .map(|e| e.to_string());
                        state.format_confirm = None;
                        settings = handle.snapshot().settings;
                        state.dirty = false;
                    }
                }
            });
        if response.should_close() {
            state.format_confirm = None;
        }
    }
    if back {
        state.page = if matches!(
            state.page,
            Page::Mouse | Page::Microphone | Page::DisplayAspect
        ) {
            Page::Advanced
        } else {
            Page::Quality
        };
        state.display.reset();
        state.dirty = false;
        state.local_error = None;
        settings = snapshot.settings;
    } else if let Some(action) = action {
        let result = match action {
            Action::Apply if handle.frame_rate_needs_super_screen(screen_id, settings) => {
                let (width, height, dpi) = topology_menu::local_parameters(ctx, handle, local_size);
                topology_menu::request(
                    ctx,
                    handle,
                    screen_id,
                    crate::features::stream_control::DisplayTopologyAction::FrameRate {
                        settings,
                        width,
                        height,
                        dpi,
                    },
                );
                settings = snapshot.settings;
                Ok(0)
            }
            Action::Apply => handle.apply(settings),
            Action::Color(enabled) | Action::Hdr(enabled) => {
                let hdr_change = matches!(action, Action::Hdr(_));
                match handle.propose_format(
                    (!hdr_change).then_some(enabled),
                    hdr_change.then_some(enabled),
                ) {
                    Ok(proposed) => {
                        let current = handle.snapshot().settings;
                        let extra_change = proposed.quality != current.quality
                            || (hdr_change && proposed.true_color != current.true_color)
                            || (!hdr_change && proposed.hdr != current.hdr);
                        if extra_change {
                            state.format_confirm = Some((screen_id, proposed));
                            Ok(0)
                        } else {
                            let result = handle.apply_format(screen_id, proposed);
                            settings = handle.snapshot().settings;
                            result
                        }
                    }
                    Err(error) => Err(error),
                }
            }
        };
        match result {
            Ok(sequence) => {
                state.local_error = None;
                state.dirty = false;
                tracing::info!(
                    sequence,
                    ?settings,
                    "runtime stream settings requested from GUI"
                );
            }
            Err(error) => state.local_error = Some(error.to_string()),
        }
    }
    state.settings = Some(settings);
    state.open = open && !close;
}
