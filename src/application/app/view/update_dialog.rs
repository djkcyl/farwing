use super::*;
use crate::application::app::updates::State;

impl DeviceCenterApp {
    pub(in crate::application::app) fn update_dialog(&mut self, ctx: &egui::Context) {
        if !self.updates.dialog_open {
            return;
        }
        let id = egui::Id::new("client-release-update");
        // Let account confirmations and existing popups finish first.
        if ctx.memory(|memory| memory.top_modal_layer().is_some_and(|layer| layer.id != id))
            || egui::Popup::is_any_open(ctx)
        {
            return;
        }
        let State::Available {
            version,
            url,
            notes,
            published,
            prerelease,
        } = &self.updates.state
        else {
            self.updates.dialog_open = false;
            return;
        };
        let (dismiss, download) =
            release_prompt(ctx, version, notes, published.as_deref(), *prerelease);
        if download {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
        if dismiss || download {
            self.updates.dialog_open = false;
        }
    }
}

fn release_prompt(
    ctx: &egui::Context,
    version: &str,
    notes: &str,
    published: Option<&str>,
    prerelease: bool,
) -> (bool, bool) {
    let mut dismiss = false;
    let mut download = false;
    let response = egui::Modal::new(egui::Id::new("client-release-update"))
        .frame(dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::UPDATE_DIALOG_WIDTH.min((ctx.content_rect().width() - 80.0).max(240.0)),
            );
            dismiss = crate::ui::controls::dialog_header(
                ui,
                if prerelease {
                    "发现测试版"
                } else {
                    "发现新版本"
                },
                crate::ui::controls::DialogIcon::Required,
                true,
            );
            ui.label(
                RichText::new(format!("Farwing v{version}"))
                    .size(theme::SECTION)
                    .strong(),
            );
            let mut subtitle = format!("当前版本 v{}", env!("CARGO_PKG_VERSION"));
            if let Some(date) = published {
                subtitle.push_str(&format!("  ·  发布于 {date}"));
            }
            ui.label(RichText::new(subtitle).size(theme::SMALL).color(MUTED));
            ui.add_space(20.0);
            ui.label(RichText::new("更新内容").strong());
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .id_salt(("release-notes", version))
                .max_height(
                    theme::RELEASE_NOTES_HEIGHT
                        .min((ctx.content_rect().height() - 280.0).max(80.0)),
                )
                .auto_shrink([false, true])
                .show(ui, |ui| crate::ui::controls::release_notes(ui, notes));
            let (accept, later) = crate::ui::controls::dialog_actions(
                ui,
                Some(crate::ui::controls::DialogAction::new("前往下载")),
                Some("稍后"),
            );
            download = accept;
            dismiss |= later;
        });
    (dismiss || response.should_close(), download)
}
