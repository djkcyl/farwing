//! Linux controller playback: the shared receive/decode loop and an egui presenter.
#[path = "decode_pipeline.rs"]
mod decode_pipeline;
#[path = "frames.rs"]
mod frames;
#[path = "present_queue.rs"]
mod present_queue;

use crate::diagnostics::performance::{PerformanceMonitor, RenderedFrameTiming};
use crate::features::stream_control::{RemoteScreen, StreamControlHandle};
use crate::i18n::t;
use crate::media::video_color::ColorMatrix;
use crate::media::{ConnectionMediaProfile, VideoCodec};
use crate::transport::rtc::{EncodedVideoFrame, VideoFrameSink, VideoTrackSource};
use crate::ui::{branding, theme};
use anyhow::Result;
use decode_pipeline::{DecoderConfig, decoder_manager};
use egui::{ColorImage, Key, Rect, TextureHandle};
use frames::*;
use present_queue::PresentQueue;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::mpsc;

#[path = "linux_view.rs"]
mod linux_view;
pub(crate) use linux_view::{Playback, ViewerWindow, viewport_builder};
#[path = "linux_screens.rs"]
mod linux_screens;
pub(crate) use linux_screens::ScreenStreams;
#[path = "annotation.rs"]
mod annotation;
#[path = "audio_view.rs"]
mod audio_view;
#[path = "hud.rs"]
mod hud;
#[path = "participants.rs"]
mod participants;
#[path = "performance_alerts.rs"]
mod performance_alerts;
#[path = "performance_panel.rs"]
mod performance_panel;
#[path = "stream_menu.rs"]
mod stream_menu;

const COMPACT_HUD_WIDTH: f32 = 140.0;
const COMPACT_METER_WIDTH: f32 = 37.0;
const COMPACT_COLUMN_GAP: f32 = 8.0;

pub(crate) struct VideoPicture {
    pub(super) track: i32,
    pub(super) binding: u64,
    frames: FrameQueue,
    performance: PerformanceMonitor,
    error: Arc<Mutex<Option<String>>>,
    sender: mpsc::UnboundedSender<EncodedVideoFrame>,
    shutdown: Arc<AtomicBool>,
    decoder: Option<std::thread::JoinHandle<()>>,
    texture: Option<TextureHandle>,
    pending: Option<(std::time::Instant, RenderedFrameTiming)>,
}

impl VideoPicture {
    pub(crate) fn start(
        ctx: egui::Context,
        viewport: egui::ViewportId,
        profile: ConnectionMediaProfile,
        track: &VideoTrackSource,
        screen: &RemoteScreen,
    ) -> Result<Self> {
        let codec = match track.metadata.codec.to_ascii_lowercase().as_str() {
            "video/h264" => VideoCodec::H264,
            value => anyhow::bail!((t().viewer.unsupported_video_codec)(value)),
        };
        let performance = track.performance.clone();
        let feedback = track.feedback.clone();
        let (sender, receiver) = mpsc::unbounded_channel();
        let frames = Arc::new(Mutex::new(PresentQueue::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let config = DecoderConfig {
            codec,
            width: screen.width,
            height: screen.height,
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
                    let wake = FrameWake::for_egui(ctx.clone(), viewport);
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
                    ctx.request_repaint_of(viewport);
                })?
        };
        Ok(Self {
            track: track.index,
            binding: screen.binding(),
            frames,
            performance,
            error,
            sender,
            shutdown,
            decoder: Some(decoder),
            texture: None,
            pending: None,
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

    pub(super) fn paint(
        &mut self,
        ui: &mut egui::Ui,
        control: &StreamControlHandle,
        screen: i32,
    ) -> Result<Option<Rect>> {
        if let Some(error) = mutex_lock(&self.error).take() {
            anyhow::bail!(error);
        }
        let frame = take_next_frame(&mut mutex_lock(&self.frames), &self.performance);
        if let Some(frame) = frame {
            let image = image(&frame)?;
            self.pending = Some((
                frame.received_at,
                RenderedFrameTiming {
                    is_new_picture: frame.is_new_picture,
                    width: frame.width,
                    height: frame.height,
                    local: Duration::ZERO,
                    surface_transfer: Duration::ZERO,
                    present_wait: Duration::ZERO,
                    render_queue: Duration::ZERO,
                    decoded_at: frame.decoded_at,
                    assembly: frame.assembly_delay,
                    input_queue: frame.input_queue_delay,
                    decode_pipeline: frame.decode_pipeline_delay,
                    sender_capture_at: frame.sender_timing.capture_at,
                    sender_capture: frame.sender_timing.capture_delay,
                    sender_encode: frame.sender_timing.encode_delay,
                    sender_pacer: frame.sender_timing.pacer_delay,
                    sender_total: frame.sender_timing.sending_delay,
                    transport: frame.sender_timing.transport_delay,
                },
            ));
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
            return Ok(None);
        };
        let available = ui.available_rect_before_wrap();
        if !available.is_positive() {
            return Ok(None);
        }
        let orientation = control.orientation(self.track);
        let [width, height] = texture.size().map(|size| size as u32);
        let (width, height) =
            control.presentation_size(self.track, orientation.turned((width, height)));
        let size = egui::vec2(width as f32, height as f32);
        let fitted = size * (available.width() / size.x).min(available.height() / size.y);
        let rect = Rect::from_center_size(available.center(), fitted);
        let uv = [
            egui::pos2(0.0, 0.0),
            egui::pos2(1.0, 0.0),
            egui::pos2(1.0, 1.0),
            egui::pos2(0.0, 1.0),
        ];
        let corners = [
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ];
        let mut mesh = egui::Mesh::with_texture(texture.id());
        for (pos, source) in corners.into_iter().zip(orientation.source_corners()) {
            mesh.vertices.push(egui::epaint::Vertex {
                pos,
                uv: uv[source],
                color: egui::Color32::WHITE,
            });
        }
        mesh.indices.extend([0, 1, 2, 0, 2, 3]);
        ui.painter().add(egui::Shape::mesh(mesh));
        if let Some((received_at, timing)) = self.pending.take() {
            let performance = self.performance.clone();
            let control = control.clone();
            ui.painter().add(egui::PaintCallback {
                rect,
                callback: Arc::new(eframe::egui_glow::CallbackFn::new(move |_, _| {
                    performance.record_rendered_frame(RenderedFrameTiming {
                        local: received_at.elapsed(),
                        render_queue: timing.decoded_at.elapsed(),
                        ..timing
                    });
                    control.topology_frame_presented(screen, received_at);
                })),
            });
        }
        Ok(Some(rect))
    }

    pub(super) fn native_size(&self, control: &StreamControlHandle) -> Option<(u32, u32)> {
        let [w, h] = self.texture.as_ref()?.size().map(|s| s as u32);
        Some(control.orientation(self.track).turned((w, h)))
    }

    pub(super) fn performance(&self) -> &PerformanceMonitor {
        &self.performance
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

impl Drop for VideoPicture {
    fn drop(&mut self) {
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
