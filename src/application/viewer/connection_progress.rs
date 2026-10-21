use super::*;
use crate::i18n::t;
use crate::ui::{controls, theme};

impl ConnectionProgressApp {
    fn draw_details(&mut self, ui: &mut egui::Ui, background: bool) {
        let failed = matches!(self.current.state, ConnectionProgressState::Failed);
        let ready = matches!(self.current.state, ConnectionProgressState::Ready);
        egui::Panel::left("connection-stages")
            .exact_size(theme::CONNECTION_STAGE_WIDTH)
            .frame(egui::Frame::new().fill(theme::SIDEBAR).inner_margin(20))
            .show(ui, |ui| {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(if failed && self.current.ended {
                        t().viewer.connection_ended
                    } else if failed {
                        t().viewer.connection_failed
                    } else if ready {
                        t().viewer.connected
                    } else {
                        t().viewer.connecting
                    })
                    .size(theme::SMALL)
                    .color(theme::MUTED),
                );
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&self.alias)
                            .size(theme::SECTION)
                            .color(theme::TEXT),
                    )
                    .wrap(),
                );
                ui.add_space(28.0);
                let t = &t().viewer;
                let stages = [
                    (1, t.stage_check_device),
                    (3, t.stage_create_session),
                    (4, t.stage_connect),
                    (6, t.stage_choose_route),
                    (9, t.stage_prepare_video),
                ];
                let active = stages
                    .iter()
                    .rposition(|(step, _)| *step <= self.current.step)
                    .unwrap_or(0);
                for (index, (_, label)) in stages.iter().enumerate() {
                    controls::connection_stage_row(
                        ui,
                        label,
                        index == active && !ready,
                        index < active || ready,
                        failed && index == active,
                    );
                }
                ui.add_space((ui.available_height() - controls::HEIGHT).max(16.0));
                if ui
                    .add(controls::secondary(if failed {
                        t.close_window
                    } else {
                        t.cancel_connection
                    }))
                    .clicked()
                {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(if background {
                        egui::Color32::TRANSPARENT
                    } else {
                        theme::BG
                    })
                    .inner_margin(24),
            )
            .show(ui, |ui| {
                let elapsed = if failed || ready {
                    self.events
                        .last()
                        .map_or(Duration::ZERO, |(elapsed, _)| *elapsed)
                } else {
                    self.started_at.elapsed()
                };
                if controls::connection_details_header(
                    ui,
                    &(t().viewer.elapsed_seconds)(elapsed.as_secs_f64()),
                ) {
                    self.details_open = false;
                    ui.ctx().request_repaint();
                }
                ui.add_space(16.0);
                ui.label(
                    egui::RichText::new(&self.current.title)
                        .size(theme::DIALOG_TITLE)
                        .color(if failed { theme::RED } else { theme::TEXT }),
                );
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&self.current.detail)
                            .size(theme::SMALL)
                            .color(theme::MUTED),
                    )
                    .wrap(),
                );
                ui.add_space(20.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(t().viewer.connection_log)
                        .size(theme::SMALL)
                        .color(theme::MUTED),
                );
                ui.add_space(8.0);
                let records = egui::ScrollArea::vertical()
                    .id_salt("connection-diagnostics")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show_viewport(ui, |ui, viewport| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let mut rows = Vec::with_capacity(self.events.len());
                        for (index, (elapsed, event)) in self.events.iter().enumerate() {
                            rows.push(controls::connection_detail_row(
                                ui,
                                &format!("{:.2}s", elapsed.as_secs_f64()),
                                &event.title,
                                &event.detail,
                                index + 1 == self.events.len(),
                                matches!(event.state, ConnectionProgressState::Failed),
                            ));
                        }
                        // Tail padding aligns automatic following to an event boundary.
                        // Keep wrapped rows measurable and retain ordinary manual scrolling.
                        if let (Some(first), Some(last)) = (rows.first(), rows.last())
                            && last.bottom() - first.top() > viewport.height()
                        {
                            if let Some(row) = rows
                                .iter()
                                .find(|row| last.bottom() - row.top() <= viewport.height())
                            {
                                let padding = viewport.height() - (last.bottom() - row.top());
                                ui.expand_to_include_rect(egui::Rect::from_min_max(
                                    last.min,
                                    egui::pos2(last.right(), last.bottom() + padding),
                                ));
                            }
                        }
                        viewport.min.y
                    });
                if (records.inner - records.state.offset.y).abs() > 0.5 {
                    ui.ctx()
                        .request_discard("connection records moved after layout");
                }
            });
    }

    pub(super) fn draw(&mut self, ui: &mut egui::Ui) {
        self.drain();
        let failed = matches!(self.current.state, ConnectionProgressState::Failed);
        let ready = matches!(self.current.state, ConnectionProgressState::Ready);
        let ctx = ui.ctx().clone();
        self.wallpapers.poll(&ctx);
        if let Some(image) = self.last_frame.take() {
            self.last_frame_texture =
                Some(ctx.load_texture("reconnect-last-frame", image, egui::TextureOptions::LINEAR));
        }
        let texture = self.last_frame_texture.clone().or_else(|| {
            self.background
                .as_ref()
                .and_then(|s| self.wallpapers.texture(&ctx, &s.device_id, &s.url))
        });
        if let Some(texture) = &texture {
            controls::connection_wallpaper(ui, texture, self.details_open);
        }
        if let Some(started) = self.update_started {
            // Keep one update presentation across readiness probes, room join,
            // decoder startup and the final wait for the first presented frame.
            // Player drops this backdrop only when presentation actually starts.
            if texture.is_none() {
                ui.painter()
                    .rect_filled(ui.available_rect_before_wrap(), 0., theme::BG);
            }
            if crate::features::remote_upgrade::progress_prompt(&ctx, started) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        }
        if !failed {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        if self.details_open {
            self.draw_details(ui, texture.is_some());
            return;
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(if texture.is_some() {
                        egui::Color32::TRANSPARENT
                    } else {
                        theme::BG
                    })
                    .inner_margin(24),
            )
            .show(ui, |ui| {
                let top = ((ui.available_height() - 320.0) / 2.0).clamp(24.0, 180.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(top);
                        let width = ui.available_width().min(theme::CONNECTION_CONTENT_WIDTH);
                        let left = ui.available_rect_before_wrap().left()
                            + (ui.available_width() - width) / 2.0;
                        let rect = egui::Rect::from_min_size(
                            egui::pos2(left, ui.cursor().top()),
                            egui::vec2(width, 0.0),
                        );
                        let mut content = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect)
                                .layout(egui::Layout::top_down(egui::Align::Center)),
                        );
                        content.set_width(width);
                        controls::connection_indicator(&mut content, failed, ready);
                        content.add_space(20.0);
                        content.add(
                            egui::Label::new(
                                egui::RichText::new(&self.alias)
                                    .size(theme::DIALOG_TITLE)
                                    .color(theme::TEXT),
                            )
                            .truncate(),
                        );
                        content.add_space(10.0);
                        let t = &t().viewer;
                        let phase = if failed && self.current.ended {
                            t.connection_ended
                        } else if failed {
                            t.connection_failed
                        } else if ready {
                            t.connected
                        } else {
                            match self.current.step {
                                0..=2 => t.phase_checking_device,
                                3..=5 => t.phase_securing,
                                6..=8 => t.phase_connecting,
                                _ => t.phase_preparing_video,
                            }
                        };
                        content.label(
                            egui::RichText::new(phase)
                                .size(theme::BODY)
                                .color(if failed { theme::RED } else { theme::MUTED }),
                        );
                        if failed {
                            content.add_space(12.0);
                            content.add(
                                egui::Label::new(
                                    egui::RichText::new(&self.current.detail)
                                        .size(theme::SMALL)
                                        .color(theme::MUTED),
                                )
                                .wrap(),
                            );
                        }
                        if !failed && let Some(retry) = self.current.retry.clone() {
                            // Recovering a lost session: say what we wait for.
                            content.add_space(12.0);
                            content.add(
                                egui::Label::new(
                                    egui::RichText::new(&self.current.detail)
                                        .size(theme::SMALL)
                                        .color(theme::MUTED),
                                )
                                .wrap(),
                            );
                            content.add_space(12.0);
                            if content.add(controls::primary(t.retry_now)).clicked() {
                                retry.trigger();
                            }
                        }
                        content.add_space(24.0);
                        if content
                            .add(controls::secondary(if failed {
                                t.close_window
                            } else {
                                t.cancel_connection
                            }))
                            .clicked()
                        {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        content.add_space(6.0);
                        if content
                            .add(controls::quiet_button(if self.details_open {
                                t.hide_connection_details
                            } else {
                                crate::i18n::t().controls.connection_details
                            }))
                            .clicked()
                        {
                            self.details_open = !self.details_open;
                            ctx.request_repaint();
                        }
                        ui.advance_cursor_after_rect(content.min_rect());
                    });
            });
    }
}
