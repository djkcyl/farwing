//! Linux controller playback: the shared receive/decode loop and an egui presenter.
#[path = "decode_pipeline.rs"]
mod decode_pipeline;
#[path = "frames.rs"]
mod frames;
#[path = "present_queue.rs"]
mod present_queue;

use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::{MouseMode, StreamControlHandle};
use crate::i18n::t;
use crate::media::video_color::ColorMatrix;
use crate::media::{ConnectionMediaProfile, VideoCodec};
use crate::transport::rtc::{EncodedVideoFrame, VideoFrameSink, VideoReceiverFeedback};
use crate::ui::{
    branding,
    controls::{ViewerCaptionIcon, viewer_caption_button},
    theme,
};
use anyhow::Result;
use decode_pipeline::{DecoderConfig, decoder_manager};
use egui::{ColorImage, Event, Key, MouseWheelUnit, PointerButton, Rect, TextureHandle};
use frames::*;
use present_queue::PresentQueue;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::mpsc;

const INPUT_OWNER: u64 = 1;

pub(crate) struct Playback {
    input: StreamControlHandle,
    screen: i32,
    frames: FrameQueue,
    performance: PerformanceMonitor,
    error: Arc<Mutex<Option<String>>>,
    sender: mpsc::UnboundedSender<EncodedVideoFrame>,
    shutdown: Arc<AtomicBool>,
    decoder: Option<std::thread::JoinHandle<()>>,
    texture: Option<TextureHandle>,
    modifiers: [bool; 4],
}

impl Playback {
    pub(crate) fn start(
        ctx: egui::Context,
        codec: VideoCodec,
        profile: ConnectionMediaProfile,
        performance: PerformanceMonitor,
        feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
        input: StreamControlHandle,
        screen: i32,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let frames = Arc::new(Mutex::new(PresentQueue::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let config = DecoderConfig {
            codec,
            width: profile.local_display.width,
            height: profile.local_display.height,
            frame_rate: profile.stream_fps,
            decoder: profile.decoder,
            software_decode: Arc::new(AtomicBool::new(true)),
            decode_enabled: Arc::new(AtomicBool::new(true)),
            decode_idle: tokio::sync::watch::channel(0).0,
            pause_epoch: Arc::new(AtomicU64::new(0)),
            observer: None,
        };
        let decoder = {
            let frames = frames.clone();
            let error = error.clone();
            let shutdown = shutdown.clone();
            let performance = performance.clone();
            std::thread::Builder::new()
                .name("farwing-video-decoder".into())
                .spawn(move || {
                    let wake = FrameWake::for_egui(ctx.clone());
                    decoder_manager(
                        config,
                        receiver,
                        frames,
                        wake,
                        performance,
                        shutdown,
                        error,
                        feedback,
                        None,
                    );
                    ctx.request_repaint();
                })?
        };
        Ok(Self {
            input,
            screen,
            frames,
            performance,
            error,
            sender,
            shutdown,
            decoder: Some(decoder),
            texture: None,
            modifiers: [false; 4],
        })
    }

    pub(crate) fn sink(&self) -> VideoFrameSink {
        VideoFrameSink::Unbounded {
            sender: self.sender.clone(),
            wake: self
                .decoder
                .as_ref()
                .expect("active decoder")
                .thread()
                .clone(),
        }
    }

    pub(super) fn toolbar(&mut self, ui: &mut egui::Ui) -> Result<()> {
        let text = t();
        let state = self.input.snapshot();
        let audio = self.input.audio();
        let mut settings = audio.settings();
        if viewer_caption_button(
            ui,
            ViewerCaptionIcon::Audio {
                muted: settings.muted,
            },
            settings.muted,
            if settings.muted {
                text.viewer.unmute
            } else {
                text.viewer.mute
            },
        )
        .clicked()
        {
            settings.muted = !settings.muted;
            audio.set_settings(settings);
        }

        let mouse = self.input.mouse();
        let control = state.mouse_mode != MouseMode::View;
        let hint = if control {
            text.viewer.shortcut_release_control
        } else if mouse.keyboard_supported() {
            text.viewer.control_keyboard_mouse
        } else {
            text.viewer.control_mouse
        };
        let picture_ready = self.texture.is_some();
        if ui
            .add_enabled_ui(picture_ready && !state.mouse_pending, |ui| {
                viewer_caption_button(ui, ViewerCaptionIcon::Mouse, control, hint)
            })
            .inner
            .on_disabled_hover_text(if !picture_ready {
                text.session.progress_waiting_picture
            } else if mouse.transport_recovering() {
                text.viewer.control_recovering
            } else {
                text.viewer.release_all_keys
            })
            .clicked()
        {
            self.input.set_mouse_mode(if control {
                MouseMode::View
            } else {
                MouseMode::Remote
            })?;
        } else if !control
            && state.remote_notice.as_ref().is_some_and(|(_, _, ask)| *ask)
            && viewer_caption_button(
                ui,
                ViewerCaptionIcon::AskControl,
                false,
                text.viewer.ask_control,
            )
            .clicked()
        {
            self.input.ask_control()?;
        }
        Ok(())
    }

    pub(super) fn notice(&self) -> Option<(String, bool)> {
        let state = self.input.snapshot();
        let audio_error = self.input.audio().snapshot().error;
        let error = state.mouse_error.is_some() || audio_error.is_some();
        let mut notices = [
            state.mouse_error,
            audio_error,
            state.remote_notice.map(|(_, notice, _)| notice),
        ]
        .into_iter()
        .flatten();
        let mut message = notices.next()?;
        for notice in notices {
            message.push('\n');
            message.push_str(&notice);
        }
        Some((message, error))
    }

    pub(super) fn picture(&mut self, ui: &mut egui::Ui) -> Result<()> {
        if let Some(error) = mutex_lock(&self.error).take() {
            anyhow::bail!(error);
        }
        let frame = take_next_frame(&mut mutex_lock(&self.frames), &self.performance);
        if let Some(frame) = frame {
            let image = image(&frame)?;
            if let Some(texture) = &mut self.texture {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture = Some(ui.ctx().load_texture(
                    "Farwing video",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
                ui.ctx().request_repaint();
            }
        }
        let Some(texture) = &self.texture else {
            waiting(ui, t().session.progress_waiting_picture);
            return Ok(());
        };
        let available = ui.available_rect_before_wrap();
        let size = texture.size_vec2();
        let fitted = size * (available.width() / size.x).min(available.height() / size.y);
        let rect = Rect::from_center_size(available.center(), fitted);
        ui.painter().image(
            texture.id(),
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        self.input(ui, rect);
        if self.input.snapshot().mouse_pending {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        Ok(())
    }

    fn input(&mut self, ui: &egui::Ui, rect: Rect) {
        let mouse = self.input.mouse();
        let state = self.input.snapshot();
        let (focused, mods, pointer, neutral, events) = ui.input(|i| {
            (
                i.focused,
                i.modifiers,
                i.pointer.hover_pos(),
                i.keys_down.is_empty() && !i.pointer.any_down() && i.modifiers.is_none(),
                i.events.clone(),
            )
        });
        if !focused || state.mouse_mode == MouseMode::View || mouse.transport_recovering() {
            mouse.pause_owner(INPUT_OWNER);
            self.modifiers = [false; 4];
            return;
        }
        let Some(screen) = state
            .remote_display
            .filter(|screen| screen.screen_id == self.screen)
        else {
            mouse.pause_owner(INPUT_OWNER);
            self.modifiers = [false; 4];
            return;
        };
        if mouse.waiting_for_neutral() {
            if neutral {
                mouse.confirm_neutral(mouse.activation_generation());
            }
            return;
        }
        let in_picture = pointer.is_some_and(|pos| rect.contains(pos));
        let move_to = |pos: egui::Pos2| {
            let p = (pos - rect.min) / rect.size();
            mouse.absolute(
                INPUT_OWNER,
                screen.screen_id,
                f64::from(p.x.clamp(0.0, 0.999_999)),
                f64::from(p.y.clamp(0.0, 0.999_999)),
            );
        };
        let modifiers = [mods.ctrl, mods.shift, mods.alt, mods.mac_cmd];
        for (i, (held, next)) in self.modifiers.iter_mut().zip(modifiers).enumerate() {
            if *held != next && (!next || in_picture) {
                mouse.key(INPUT_OWNER, 0x0007_00e0 + i as u32, next);
                *held = next;
            }
        }
        for event in events {
            match event {
                Event::PointerMoved(pos) if rect.contains(pos) => move_to(pos),
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } if !pressed || rect.contains(pos) => {
                    if rect.contains(pos) {
                        move_to(pos);
                    }
                    let button = match button {
                        PointerButton::Primary => 1,
                        PointerButton::Secondary => 2,
                        PointerButton::Middle => 16,
                        PointerButton::Extra1 => 32,
                        PointerButton::Extra2 => 64,
                    };
                    mouse.button(INPUT_OWNER, button, pressed);
                }
                Event::Key {
                    physical_key: Some(physical),
                    pressed,
                    ..
                } if !pressed || in_picture => {
                    if let Some(usage) = key_usage(physical) {
                        mouse.key(INPUT_OWNER, usage, pressed);
                    }
                }
                Event::MouseWheel { unit, delta, .. } if in_picture => {
                    let scale = match unit {
                        MouseWheelUnit::Point => 1.0,
                        MouseWheelUnit::Line => 120.0,
                        MouseWheelUnit::Page => 360.0,
                    };
                    mouse.wheel(
                        INPUT_OWNER,
                        (delta.x * scale).round() as i32,
                        (delta.y * scale).round() as i32,
                    );
                }
                _ => {}
            }
        }
    }
}

pub(super) fn waiting(ui: &mut egui::Ui, stage: &str) {
    let bounds = ui.available_rect_before_wrap();
    let logo = branding::logo_size(theme::LOGO_LOADING);
    let font = egui::FontId::proportional(theme::BODY);
    let line_height = ui.fonts_mut(|fonts| fonts.row_height(&font));
    let spinner_size = theme::SECTION;
    let gap = 10.0;
    let height = logo.y + spinner_size + line_height + 2.0 * gap;
    let rect = Rect::from_center_size(
        bounds.center(),
        egui::vec2(bounds.width().min(theme::CONNECTION_CONTENT_WIDTH), height),
    );
    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Center)),
    );
    content.spacing_mut().item_spacing.y = gap;
    let (logo_rect, _) = content.allocate_exact_size(logo, egui::Sense::hover());
    branding::paint_logo(
        content.painter(),
        logo_rect.left(),
        logo_rect.center().y,
        theme::LOGO_LOADING,
        theme::TEXT,
    );
    content.add(egui::Spinner::new().size(spinner_size).color(theme::ACCENT));
    content.label(egui::RichText::new(stage).font(font).color(theme::MUTED));
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.input.mouse().pause_owner(INPUT_OWNER);
        self.shutdown.store(true, Ordering::Release);
        if let Some(decoder) = self.decoder.take() {
            decoder.thread().unpark();
            let _ = decoder.join();
        }
    }
}

fn image(frame: &DecodedVideoFrame) -> Result<ColorImage> {
    let (width, height) = frame.surface.size();
    let (y, uv) = frame.surface.planes();
    let input = yuv::YuvBiPlanarImage {
        y_plane: y,
        y_stride: width,
        uv_plane: uv,
        uv_stride: width,
        width,
        height,
    };
    let mut pixels = vec![0; width as usize * height as usize * 4];
    yuv::yuv_nv12_to_rgba(
        &input,
        &mut pixels,
        width * 4,
        if frame.color.full_range {
            yuv::YuvRange::Full
        } else {
            yuv::YuvRange::Limited
        },
        match frame.color.matrix {
            ColorMatrix::Bt601 => yuv::YuvStandardMatrix::Bt601,
            ColorMatrix::Bt709 => yuv::YuvStandardMatrix::Bt709,
            ColorMatrix::Bt2020 => yuv::YuvStandardMatrix::Bt2020,
        },
        yuv::YuvConversionMode::Balanced,
    )?;
    Ok(ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &pixels,
    ))
}

fn key_usage(key: Key) -> Option<u32> {
    use Key::*;
    let usage = match key {
        _ if (A as u32..=Z as u32).contains(&(key as u32)) => key as u32 - A as u32 + 0x04,
        _ if (Num1 as u32..=Num9 as u32).contains(&(key as u32)) => key as u32 - Num1 as u32 + 0x1e,
        Num0 => 0x27,
        _ if (F1 as u32..=F12 as u32).contains(&(key as u32)) => key as u32 - F1 as u32 + 0x3a,
        _ if (F13 as u32..=F24 as u32).contains(&(key as u32)) => key as u32 - F13 as u32 + 0x68,
        Enter => 0x28,
        Escape => 0x29,
        Backspace => 0x2a,
        Tab => 0x2b,
        Space => 0x2c,
        Minus => 0x2d,
        Equals => 0x2e,
        OpenBracket => 0x2f,
        CloseBracket => 0x30,
        Backslash => 0x31,
        Semicolon => 0x33,
        Quote => 0x34,
        Backtick => 0x35,
        Comma => 0x36,
        Period => 0x37,
        Slash => 0x38,
        Insert => 0x49,
        Home => 0x4a,
        PageUp => 0x4b,
        Delete => 0x4c,
        End => 0x4d,
        PageDown => 0x4e,
        ArrowRight => 0x4f,
        ArrowLeft => 0x50,
        ArrowDown => 0x51,
        ArrowUp => 0x52,
        _ => return None,
    };
    Some(crate::platform::keyboard::KEYBOARD | usage)
}
