//! A voice channel's or call's picture in a window of its own
//! (`docs/protocol/chat-design.md` §8.2): it keeps coming while the window
//! is open; a double click or F11 makes it full screen, Esc ends that.

use crate::features::chat::ChatService;
use crate::ui::window_manager;
use egui::{Color32, Rect, vec2};
use std::sync::Mutex;

/// The tracks the open windows show (the stage keeps receiving them).
static POPPED: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());

fn popped() -> std::sync::MutexGuard<'static, Vec<(u32, String)>> {
    POPPED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The tracks open windows show, to keep receiving.
pub(super) fn watched() -> Vec<(u32, String)> {
    popped().clone()
}

/// Opens `participant`'s picture (`kinds`, best first) in a window named
/// `title`, receiving `tracks` while it is open.
pub(super) fn open(chat: ChatService, participant: u32, kinds: [&'static str; 2], tracks: Vec<(u32, String)>, title: String) {
    let key = format!("chat-picture:{participant}:{}", kinds[0]);
    let viewport = egui::ViewportBuilder::default()
        .with_title(format!("{} · {title}", crate::APP_NAME))
        .with_icon(crate::ui::branding::icon())
        .with_inner_size(vec2(960.0, 560.0))
        .with_min_inner_size(vec2(320.0, 200.0));
    popped().extend(tracks.iter().cloned());
    let result = window_manager::send(window_manager::Request::Open {
        key,
        config: crate::ui::WindowConfig {
            viewport,
            centered: true,
            notification: false,
        },
        factory: Box::new(move |ctx, _| {
            crate::ui::theme::configure(ctx);
            Box::new(PictureWindow {
                chat,
                participant,
                kinds,
                tracks,
                full: false,
            })
        }),
    });
    if result.is_err() {
        popped().clear();
    }
}

struct PictureWindow {
    chat: ChatService,
    participant: u32,
    kinds: [&'static str; 2],
    tracks: Vec<(u32, String)>,
    full: bool,
}

impl crate::ui::App for PictureWindow {
    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let rect = ctx.content_rect();
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        // The picture is gone (the voice or call ended, the share stopped).
        let picture = self.kinds.iter().find_map(|kind| self.chat.picture(self.participant, kind));
        if self.chat.view().my_voice.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let shown = picture.and_then(|p| Some((p, crate::ui::video_textures::texture(p.handle)?)));
        match shown {
            Some((picture, id)) => {
                let size = vec2(picture.width.max(1) as f32, picture.height.max(1) as f32);
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let fitted = Rect::from_center_size(rect.center(), size * scale);
                ui.painter().image(id, fitted, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                ui.put(Rect::from_center_size(rect.center(), vec2(24.0, 24.0)), egui::Spinner::new());
            }
        }
        let response = ui.interact(rect, ui.id().with("chat-picture"), egui::Sense::click());
        let toggle = response.double_clicked() || ctx.input(|i| i.key_pressed(egui::Key::F11));
        let leave = self.full && ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if toggle || leave {
            self.full = !self.full && !leave;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.full));
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }

    fn on_exit(&mut self) {
        popped().retain(|t| !self.tracks.contains(t));
    }
}
