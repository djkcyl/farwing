use crate::i18n::t;
use crate::media::audio::encoder::Quality;

fn label(kbps: u32) -> &'static str {
    let t = &t().controls;
    match kbps {
        64 => t.audio_quality_64,
        128 => t.audio_quality_128,
        192 => t.audio_quality_192,
        256 => t.audio_quality_256,
        _ => t.audio_quality_unselected,
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
            egui::RichText::new(t().controls.received_audio_quality)
                .size(super::theme::SMALL)
                .color(super::theme::MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let note = if status.pending {
                t().controls.applying.to_owned()
            } else if status.available && status.effective_bps < status.quality.kbps * 1000 {
                (t().controls.negotiated_limit)(status.effective_bps / 1000)
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
            egui::RichText::new(t().controls.audio_quality_set_by_host)
                .size(super::theme::SMALL)
                .color(super::theme::MUTED),
        );
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
            egui::RichText::new(t().controls.audio_quality)
                .size(super::theme::SMALL)
                .color(super::theme::MUTED),
        );
        if !status.available {
            ui.label(
                egui::RichText::new(t().controls.set_by_host)
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
