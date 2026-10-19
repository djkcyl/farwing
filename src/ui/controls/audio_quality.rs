use crate::media::audio::encoder::Quality;

fn label(kbps: u32) -> &'static str {
    match kbps {
        64 => "省带宽 · 64 kbps",
        128 => "标准 · 128 kbps",
        192 => "高音质 · 192 kbps",
        256 => "极高音质 · 256 kbps",
        _ => "请选择音质档位",
    }
}

pub(crate) fn audio_quality(ui: &mut egui::Ui, quality: &mut Quality) -> bool {
    let previous = *quality;
    egui::ComboBox::from_id_salt("audio-quality")
        .width(238.0)
        .selected_text(label(quality.kbps))
        .show_ui(ui, |ui| {
            for kbps in Quality::RATES {
                ui.selectable_value(&mut quality.kbps, kbps, label(kbps));
            }
        });
    *quality != previous
}

pub(crate) fn audio_quality_segments(ui: &mut egui::Ui, quality: &mut Quality) -> bool {
    quality_segments(ui, quality, true)
}
fn quality_segments(ui: &mut egui::Ui, quality: &mut Quality, tooltips: bool) -> bool {
    let previous = *quality;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - ui.spacing().item_spacing.x * 3.0) / 4.0;
        for kbps in Quality::RATES {
            let response = ui.add_sized(
                [width, super::theme::MENU_HEIGHT],
                egui::Button::new(kbps.to_string()).selected(quality.kbps == kbps),
            );
            let response = if tooltips {
                response.on_hover_text(label(kbps))
            } else {
                response
            };
            if response.clicked() {
                quality.kbps = kbps;
            }
        }
    });
    *quality != previous
}

/// The receiving side only offers changes after the host advertises the extension.
pub(crate) fn remote_audio_quality(
    ui: &mut egui::Ui,
    handle: &crate::features::stream_control::StreamControlHandle,
) {
    let status = handle.audio_quality_snapshot();
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("接收音质 · kbps")
                .size(super::theme::SMALL)
                .color(super::theme::MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let note = if status.pending {
                "正在应用…".to_owned()
            } else if status.available && status.effective_bps < status.quality.kbps * 1000 {
                format!("协商上限 {} kbps", status.effective_bps / 1000)
            } else {
                String::new()
            };
            ui.label(
                egui::RichText::new(note)
                    .size(super::theme::TINY)
                    .color(super::theme::MUTED),
            );
        });
    });
    if !status.available {
        ui.label(
            egui::RichText::new("音质由被控端设置")
                .size(super::theme::SMALL)
                .color(super::theme::MUTED),
        )
        .on_hover_text("当前被控端未提供音质切换，需要两端均使用支持此功能的 Farwing。");
        return;
    }
    let mut quality = status.quality;
    if ui
        .add_enabled_ui(!status.pending, |ui| {
            audio_quality_segments(ui, &mut quality)
        })
        .inner
    {
        if let Err(error) = handle.set_audio_quality(quality) {
            ui.label(egui::RichText::new(error.to_string()).color(super::theme::AMBER));
        }
    }
    if let Some(error) = status.error {
        ui.label(
            egui::RichText::new(error)
                .size(super::theme::SMALL)
                .color(super::theme::AMBER),
        );
    }
}

/// Single-row variant for the fixed audio player; feedback never shifts controls.
pub(crate) fn remote_audio_quality_inline(
    ui: &mut egui::Ui,
    handle: &crate::features::stream_control::StreamControlHandle,
) {
    let status = handle.audio_quality_snapshot();
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("音质")
                .size(super::theme::SMALL)
                .color(super::theme::MUTED),
        );
        if !status.available {
            ui.label(
                egui::RichText::new("由被控端设置")
                    .size(super::theme::SMALL)
                    .color(super::theme::MUTED),
            );
            return;
        }
        let mut quality = status.quality;
        let response = ui.add_enabled_ui(!status.pending, |ui| {
            quality_segments(ui, &mut quality, false)
        });
        if response.inner {
            let _ = handle.set_audio_quality(quality);
        }
    });
}
