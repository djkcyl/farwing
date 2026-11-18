use crate::media::selection::GpuId;
use crate::platform::capture::EncodingAdapter;

pub(super) fn limits_title(
    mode: crate::media::selection::ProcessingMode,
    codec: Option<&str>,
) -> String {
    let mut text = "高级限制".to_owned();
    if mode != crate::media::selection::ProcessingMode::Automatic {
        text.push_str(" · ");
        text.push_str(mode.label());
    }
    if let Some(codec) = codec {
        text.push_str(" · ");
        text.push_str(codec);
    }
    text
}

pub(super) struct Inventory {
    adapters: Vec<EncodingAdapter>,
    pending: Option<std::sync::mpsc::Receiver<anyhow::Result<Vec<EncodingAdapter>>>>,
    error: Option<String>,
}
impl Inventory {
    pub fn new() -> Self {
        let mut value = Self {
            adapters: vec![],
            pending: None,
            error: None,
        };
        value.refresh();
        value
    }
    fn refresh(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending = Some(rx);
        let _ = std::thread::Builder::new()
            .name("media-adapters".into())
            .spawn(move || {
                let _ = tx.send(crate::platform::capture::encoding_adapters());
            });
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        id: &str,
        selected: &mut Option<GpuId>,
        encode: bool,
    ) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    match result {
                        Ok(a) => {
                            self.adapters = a;
                            self.error = None;
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.error = Some("读取显卡列表失败".into());
                }
                Err(_) => {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(100));
                }
            }
        }
        // Linux lists none: NVENC and VA-API use the GPU the desktop runs on.
        if selected.is_none()
            && self.pending.is_none()
            && self.error.is_none()
            && !self.adapters.iter().any(|a| a.id.is_some())
        {
            ui.label("自动（没有可指定的显卡）");
            return;
        }
        let repeated: std::collections::HashSet<_> = self
            .adapters
            .iter()
            .filter(|a| self.adapters.iter().filter(|b| b.name == a.name).count() > 1)
            .map(|a| a.name.clone())
            .collect();
        let label = |a: &EncodingAdapter| {
            let backend = if encode {
                match a.vendor {
                    0x10de => "NVENC",
                    0x1002 => "AMF",
                    0x8086 => "QSV",
                    _ => "硬编",
                }
            } else {
                "DXVA11"
            };
            let location = if repeated.contains(&a.name) {
                a.id.map(|id| {
                    format!(
                        " · PCI {}:{}:{}",
                        id.location[0], id.location[1], id.location[2]
                    )
                })
                .unwrap_or_default()
            } else {
                String::new()
            };
            format!("{} · {backend}{location}", a.name)
        };
        let text = selected.map_or_else(
            || "自动".to_owned(),
            |id| {
                self.adapters
                    .iter()
                    .find(|a| a.id == Some(id))
                    .map_or_else(|| "已选显卡不可用（自动回退）".to_owned(), label)
            },
        );
        egui::ComboBox::from_id_salt(id)
            .width(238.)
            .truncate()
            .selected_text(text)
            .show_ui(ui, |ui| {
                ui.selectable_value(selected, None, "自动");
                for adapter in &self.adapters {
                    if let Some(key) = adapter.id {
                        ui.selectable_value(selected, Some(key), label(adapter));
                    }
                }
                ui.separator();
                if ui.selectable_label(false, "刷新显卡列表").clicked() {
                    self.refresh();
                }
            });
        if let Some(error) = &self.error {
            ui.colored_label(crate::ui::theme::RED, error);
        }
    }
}
