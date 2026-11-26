//! File selection for chat attachments, avatars and group icons.
use std::path::PathBuf;

pub(super) struct FilePicker {
    #[cfg(windows)]
    receiver: std::sync::mpsc::Receiver<Vec<PathBuf>>,
    #[cfg(target_os = "linux")]
    title: &'static str,
    #[cfg(target_os = "linux")]
    path: String,
    #[cfg(target_os = "linux")]
    invalid: bool,
}

impl FilePicker {
    pub(super) fn open(title: &'static str) -> Self {
        #[cfg(windows)]
        return Self { receiver: crate::platform::windows::file_dialog::pick_files(title.into()) };
        #[cfg(target_os = "linux")]
        Self { title, path: String::new(), invalid: false }
    }

    /// None while open, Some(empty) when cancelled.
    pub(super) fn poll(&mut self, ctx: &egui::Context) -> Option<Vec<PathBuf>> {
        #[cfg(windows)]
        return match self.receiver.try_recv() {
            Ok(paths) => Some(paths),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Vec::new()),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                None
            }
        };
        #[cfg(target_os = "linux")]
        {
            use crate::i18n::{Language, language, t};
            use crate::ui::controls::{DialogAction, DialogIcon, dialog_actions, dialog_frame, dialog_header};

            let (hint, missing) = match language() {
                Language::ZhCn => ("输入或粘贴文件的绝对路径", "请选择存在的普通文件。"),
                Language::En => ("Enter or paste an absolute file path", "Choose an existing regular file."),
            };
            let mut picked = None;
            let modal = egui::Modal::new(egui::Id::new(("chat-pick-file", self.title))).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(420.0);
                if dialog_header(ui, self.title, DialogIcon::Files, true) {
                    picked = Some(Vec::new());
                }
                let field = ui.add(egui::TextEdit::singleline(&mut self.path).hint_text(hint).desired_width(f32::INFINITY));
                if field.changed() {
                    self.invalid = false;
                }
                if self.invalid {
                    ui.colored_label(crate::ui::theme::RED, missing);
                }
                let (open, cancel) = dialog_actions(ui, Some(DialogAction::new(t().chat.open)), Some(t().common.cancel));
                if open || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                    let path = PathBuf::from(&self.path);
                    if path.is_absolute() && path.is_file() {
                        picked = Some(vec![path]);
                    } else {
                        self.invalid = true;
                        field.request_focus();
                    }
                }
                if cancel {
                    picked = Some(Vec::new());
                }
            });
            picked.or_else(|| modal.should_close().then(Vec::new))
        }
    }
}
