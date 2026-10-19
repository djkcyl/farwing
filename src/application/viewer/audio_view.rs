//! Live audio player. Visual meters read playback measurements, not synthetic animation.
use crate::{
    features::stream_control::StreamControlHandle,
    ui::{controls, theme},
};
use egui::{Align, Align2, FontId, Layout, Rect, RichText, Sense, Stroke, pos2, vec2};
use std::time::{Duration, Instant};

mod waveform;

pub(crate) struct AudioView {
    pub control: StreamControlHandle,
    pub error: Option<String>,
    opened: Instant,
    waveform: Option<crate::media::audio::waveform::Reader>,
    waveform_zoom: f32,
    compact: bool,
    layout_change: Option<bool>,
    levels: [f32; 2],
    peaks: [(f32, f64); 2],
    updated: Option<f64>,
}

fn level_db(level: f32) -> f32 {
    if level.is_finite() && level > 0.0 {
        (20.0 * level.log10()).clamp(-60.0, 0.0)
    } else {
        -60.0
    }
}
fn db_label(level: f32) -> String {
    if level <= -60.0 {
        "−∞".into()
    } else {
        format!("{level:.1}")
    }
}

impl AudioView {
    pub(crate) fn new(control: StreamControlHandle, error: Option<String>) -> Self {
        let waveform = Some(control.audio().waveform());
        Self {
            control,
            error,
            opened: Instant::now(),
            waveform,
            waveform_zoom: 1.0,
            compact: false,
            layout_change: None,
            levels: [0.0; 2],
            peaks: [(0.0, 0.0); 2],
            updated: None,
        }
    }

    fn sample(&mut self, now: f64, input: [f32; 2]) {
        let dt = self.updated.map_or(0.0, |last| now - last);
        if !(0.0..=0.5).contains(&dt) {
            self.levels = [0.0; 2];
            self.peaks = [(0.0, 0.0); 2];
        }
        let input = input.map(|v| (level_db(v) + 60.0) / 60.0);
        for channel in 0..2 {
            // Instant attack with display-only release and one-second peak hold.
            self.levels[channel] =
                input[channel].max((self.levels[channel] - dt.max(0.0) as f32 * 1.5).max(0.0));
            if input[channel] >= self.peaks[channel].0 || now - self.peaks[channel].1 >= 1.0 {
                self.peaks[channel] = (input[channel], now);
            }
        }
        self.updated = Some(now);
    }

    pub(super) fn take_layout_change(&mut self) -> Option<bool> {
        self.layout_change.take()
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) -> bool {
        ui.ctx().request_repaint_after(Duration::from_millis(33));
        let audio = self.control.audio();
        let status = audio.snapshot();
        let mut settings = audio.settings();
        if !ui.ctx().egui_wants_keyboard_input() && !egui::Popup::is_any_open(ui.ctx()) {
            let mut toggle = false;
            ui.input_mut(|input| {
                input.events.retain(|event| {
                    if let egui::Event::Key {
                        key: egui::Key::Space,
                        pressed: true,
                        repeat,
                        modifiers,
                        ..
                    } = event
                        && modifiers.is_none()
                    {
                        toggle |= !repeat;
                        return false;
                    }
                    true
                })
            });
            if toggle {
                settings.muted = !settings.muted;
            }
        }
        let now = ui.input(|i| i.time);
        self.sample(now, audio.input_levels());
        let mut close = false;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(if self.compact { 10 } else { 14 }),
            )
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                ui.spacing_mut().interact_size.y = 28.0;
                if !self.compact {
                    self.status_row(ui, &status, settings.muted, true);
                    egui::Frame::new()
                        .fill(theme::SIDEBAR)
                        .corner_radius(6.0)
                        .stroke(Stroke::new(1.0, theme::LINE))
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = 4.0;
                            ui.spacing_mut().interact_size.y = 18.0;
                            self.meters(ui);
                            self.trace(ui);
                        });
                }
                Self::playback_controls(ui, &mut settings);
                audio.set_settings(settings);
                if !self.compact {
                    controls::remote_audio_quality_inline(ui, &self.control);
                    Self::output_device(ui, &audio, &status.device);
                }
                ui.horizontal(|ui| {
                    if self.compact {
                        self.status_row(ui, &status, settings.muted, false);
                    } else {
                        let seconds = self.opened.elapsed().as_secs();
                        ui.label(
                            RichText::new(format!(
                                "{:02}:{:02}:{:02}",
                                seconds / 3600,
                                seconds / 60 % 60,
                                seconds % 60
                            ))
                            .size(theme::SMALL)
                            .color(theme::MUTED),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if !self.compact {
                            close = ui.button("断开").clicked();
                        }
                        if ui.button("恢复画面").clicked() {
                            self.control.request_audio_only(false);
                        }
                        if ui
                            .button(if self.compact {
                                "完整模式"
                            } else {
                                "精简模式"
                            })
                            .clicked()
                        {
                            self.compact = !self.compact;
                            self.layout_change = Some(self.compact);
                            self.waveform = if self.compact {
                                None
                            } else {
                                Some(audio.waveform())
                            };
                        }
                    });
                });
            });
        close
    }

    fn status_row(
        &self,
        ui: &mut egui::Ui,
        status: &crate::media::audio::AudioSnapshot,
        muted: bool,
        details: bool,
    ) {
        let output = self.control.audio().output_devices();
        let quality = self.control.audio_quality_snapshot();
        let error = self
            .error
            .as_ref()
            .or(status.error.as_ref())
            .or(output.error.as_ref())
            .or(quality.error.as_ref());
        let text = if error.is_some() {
            "音频提示"
        } else if muted {
            "已暂停"
        } else if status.receiving {
            "正在播放"
        } else {
            "等待音频"
        };
        let color = if error.is_some() {
            theme::AMBER
        } else if muted || !status.receiving {
            theme::MUTED
        } else {
            theme::GREEN
        };
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(vec2(6.0, 6.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 2.5, color);
            if let Some(error) = error {
                if !details {
                    ui.label(RichText::new(text).size(theme::SMALL).color(color));
                } else {
                    ui.menu_button(RichText::new(text).size(theme::SMALL).color(color), |ui| {
                        ui.set_width(280.0);
                        egui::ScrollArea::vertical()
                            .max_height(150.0)
                            .show(ui, |ui| {
                                ui.label(error);
                            });
                        if status.error.is_some() && ui.button("重试音频设备").clicked() {
                            self.control.audio().retry();
                            ui.close();
                        }
                    });
                }
            } else {
                ui.label(RichText::new(text).size(theme::SMALL).color(color));
            }
            let latency = match self.control.network_control().snapshot().round_trip_ms {
                Some(ms) if ms < 1.0 => "RTT <1 ms".to_owned(),
                Some(ms) => format!("RTT {ms:.0} ms"),
                None => "RTT —".to_owned(),
            };
            ui.label(
                RichText::new(latency)
                    .size(theme::SMALL)
                    .color(theme::MUTED),
            );
            if details {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(if status.receiving {
                            "Opus · 48 kHz · 双声道"
                        } else {
                            "实时音频"
                        })
                        .size(theme::TINY)
                        .color(theme::MUTED),
                    );
                });
            }
        });
    }

    fn playback_controls(ui: &mut egui::Ui, settings: &mut crate::media::audio::AudioSettings) {
        ui.horizontal(|ui| {
            let response = ui.add_sized(
                vec2(36.0, 36.0),
                egui::Button::new("")
                    .fill(theme::ACCENT)
                    .corner_radius(18.0),
            );
            let c = response.rect.center();
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    true,
                    if settings.muted {
                        "继续监听"
                    } else {
                        "暂停监听"
                    },
                )
            });
            if settings.muted {
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        c + vec2(-4.0, -8.0),
                        c + vec2(8.0, 0.0),
                        c + vec2(-4.0, 8.0),
                    ],
                    theme::TEXT,
                    Stroke::NONE,
                ));
            } else {
                for x in [-4.0, 4.0] {
                    ui.painter().rect_filled(
                        Rect::from_center_size(c + vec2(x, 0.0), vec2(3.0, 15.0)),
                        1.0,
                        theme::TEXT,
                    );
                }
            }
            // Egui's focused-button activation also accepts modified/repeated
            // Space. Keep OS shortcuts and held keys out of playback toggles.
            let other_key = ui.input(|input| {
                input.events.iter().any(|event| {
                    matches!(event, egui::Event::Key {
                    key: egui::Key::Space | egui::Key::Enter, pressed:true,
                    repeat, modifiers, ..
                } if *repeat || !modifiers.is_none())
                })
            });
            if response.clicked() && !other_key {
                settings.muted = !settings.muted;
            }
            ui.spacing_mut().slider_width = (ui.available_width() - 58.0).max(80.0);
            ui.add(
                egui::Slider::new(&mut settings.volume, 0..=crate::media::audio::MAX_VOLUME)
                    .show_value(false)
                    .trailing_fill(true)
                    .handle_shape(egui::style::HandleShape::Circle),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!("{}%", settings.volume)).size(theme::SECTION),
                        )
                        .frame(false),
                    )
                    .double_clicked()
                {
                    settings.volume = 100;
                }
            });
        });
    }

    fn output_device(
        ui: &mut egui::Ui,
        audio: &crate::media::audio::AudioPlayback,
        device_name: &str,
    ) {
        let outputs = audio.output_devices();
        let selected = audio.selected_output();
        egui::Frame::new()
            .fill(theme::SURFACE)
            .corner_radius(6.0)
            .stroke(Stroke::new(1.0, theme::LINE))
            .inner_margin(2)
            .show(ui, |ui| {
                let name = selected.as_ref().map_or_else(
                    || {
                        if device_name.is_empty() {
                            "跟随系统默认"
                        } else {
                            device_name
                        }
                    },
                    |id| {
                        outputs
                            .devices
                            .iter()
                            .find(|d| &d.id == id)
                            .map_or("所选设备暂不可用", |d| d.name.as_str())
                    },
                );
                let response = controls::menu_row(
                    ui,
                    name,
                    if selected.is_none() {
                        "系统默认"
                    } else {
                        "指定设备"
                    },
                    None,
                    true,
                    true,
                );
                if !outputs.loaded || response.clicked() {
                    let _ = audio.refresh_output_devices();
                }
                egui::Popup::menu(&response)
                    .width(response.rect.width())
                    .gap(4.0)
                    .show(|ui| {
                        super::stream_menu::menu_style(ui);
                        if controls::menu_row(
                            ui,
                            "跟随系统默认",
                            "",
                            Some(selected.is_none()),
                            true,
                            false,
                        )
                        .clicked()
                        {
                            audio.set_output_device(None);
                            ui.close();
                        }
                        ui.separator();
                        let outputs = audio.output_devices();
                        egui::ScrollArea::vertical()
                            .max_height(168.0)
                            .show(ui, |ui| {
                                for device in &outputs.devices {
                                    if controls::menu_row(
                                        ui,
                                        &device.name,
                                        "",
                                        Some(selected.as_ref() == Some(&device.id)),
                                        true,
                                        false,
                                    )
                                    .clicked()
                                    {
                                        audio.set_output_device(Some(device.id.clone()));
                                        ui.close();
                                    }
                                }
                                if outputs.loaded && outputs.devices.is_empty() {
                                    ui.label(RichText::new("暂无可用播放设备").color(theme::MUTED));
                                }
                            });
                    });
            });
    }

    fn meters(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 54.0), Sense::hover());
        let p = ui.painter();
        let start = rect.left() + 22.0;
        let end = rect.right() - 42.0;
        let x = |n: f32| start + (end - start) * n;
        for channel in 0..2 {
            let y = rect.top() + channel as f32 * 18.0;
            p.text(
                pos2(rect.left(), y + 8.0),
                Align2::LEFT_CENTER,
                if channel == 0 { "L" } else { "R" },
                FontId::monospace(theme::SMALL),
                theme::MUTED,
            );
            for segment in 0..48 {
                let fraction = segment as f32 / 48.0;
                let color = if self.levels[channel] > fraction {
                    if fraction >= 0.95 {
                        theme::RED
                    } else if fraction >= 0.8 {
                        theme::AMBER
                    } else {
                        theme::GREEN
                    }
                } else {
                    theme::LINE
                };
                p.rect_filled(
                    Rect::from_min_max(
                        pos2(x(fraction), y),
                        pos2(x((segment + 1) as f32 / 48.0) - 1.0, y + 12.0),
                    ),
                    1.0,
                    color,
                );
            }
            if self.peaks[channel].0 > 0.0 {
                let at = x(self.peaks[channel].0);
                p.line_segment(
                    [pos2(at, y), pos2(at, y + 12.0)],
                    Stroke::new(1.5, theme::TEXT),
                );
            }
            p.text(
                pos2(rect.right(), y + 8.0),
                Align2::RIGHT_CENTER,
                db_label(self.levels[channel] * 60.0 - 60.0),
                FontId::monospace(theme::SMALL),
                theme::TEXT,
            );
        }
        for db in [-60, -48, -36, -24, -12, -6, 0] {
            p.text(
                pos2(x((db as f32 + 60.0) / 60.0), rect.top() + 40.0),
                Align2::CENTER_TOP,
                db.to_string(),
                FontId::monospace(theme::TINY),
                theme::MUTED,
            );
        }
        p.text(
            pos2(rect.right(), rect.top() + 40.0),
            Align2::RIGHT_TOP,
            "dBFS",
            FontId::monospace(theme::TINY),
            theme::MUTED,
        );
    }

    fn trace(&mut self, ui: &mut egui::Ui) {
        use crate::media::audio::waveform::SECONDS;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 90.0), Sense::hover());
        let plot = Rect::from_min_max(rect.min + vec2(22.0, 0.0), rect.max);
        let pixels_per_point = ui.ctx().pixels_per_point().max(0.5);
        let stride =
            ((SECONDS * 100.0 / (plot.width() * pixels_per_point).max(1.0)).ceil() as u64).max(1);
        let mut columns = Vec::new();
        if let Some(reader) = &mut self.waveform {
            let history = reader.snapshot();
            if let Some(last) = history.back() {
                let idle = last.at.elapsed().as_secs_f64();
                for bucket in
                    waveform::buckets(history.iter().map(|b| (b.sequence, b.min, b.max)), stride)
                {
                    let age = idle
                        + (last.sequence.saturating_sub(bucket.last_sequence) as f64
                            + (stride - 1) as f64 * 0.5)
                            * 0.01;
                    let x = plot.right() - (age / f64::from(SECONDS)) as f32 * plot.width();
                    columns.push((x, bucket.min, bucket.max));
                }
            }
        }
        let p = ui.painter().with_clip_rect(rect);
        for channel in 0..2 {
            let lane = Rect::from_min_size(
                plot.min + vec2(0.0, channel as f32 * 46.0),
                vec2(plot.width(), 38.0),
            );
            let center = lane.center().y;
            let amplitude = lane.height() * 0.5;
            let color = if channel == 0 {
                theme::GREEN
            } else {
                theme::ACCENT
            };
            p.text(
                pos2(rect.left(), center),
                Align2::LEFT_CENTER,
                if channel == 0 { "L" } else { "R" },
                FontId::monospace(theme::SMALL),
                color,
            );
            for y in [lane.top(), center, lane.bottom()] {
                p.line_segment(
                    [pos2(lane.left(), y), pos2(lane.right(), y)],
                    Stroke::new(0.5, theme::LINE),
                );
            }
            let top: Vec<_> = columns
                .iter()
                .map(|(x, _, max)| {
                    pos2(
                        *x,
                        center - (max[channel] * self.waveform_zoom).clamp(-1.0, 1.0) * amplitude,
                    )
                })
                .collect();
            let bottom: Vec<_> = columns
                .iter()
                .map(|(x, min, _)| {
                    pos2(
                        *x,
                        center - (min[channel] * self.waveform_zoom).clamp(-1.0, 1.0) * amplitude,
                    )
                })
                .collect();
            p.with_clip_rect(lane.intersect(ui.clip_rect()))
                .add(egui::Shape::mesh(waveform::envelope(
                    &top,
                    &bottom,
                    color,
                    1.0 / pixels_per_point,
                )));
        }
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("接收波形 · 最近 8 秒")
                    .size(theme::TINY)
                    .color(theme::MUTED),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button(format!("{:.0}×", self.waveform_zoom))
                    .clicked()
                {
                    self.waveform_zoom = if self.waveform_zoom >= 8.0 {
                        1.0
                    } else {
                        self.waveform_zoom * 2.0
                    };
                }
            });
        });
    }
}
