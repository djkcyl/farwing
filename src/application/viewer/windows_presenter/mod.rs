//! Windows viewer composition. Window, render worker and GPU resource owners are separate.

use crate::application::viewer::{ConnectionProgress, NativeViewerSession, ViewerDisplayHandle};
pub(super) use crate::application::viewer_shortcuts::Action as ViewerShortcut;
use crate::diagnostics::performance::RenderedFrameTiming;
use crate::features::stream_control::StreamControlHandle;
use crate::media::video_color::RenderColor;
use crate::platform::{swapchain, video_shader as shader};
use crate::ui::chrome::configure_dwm_window;
pub(super) use crate::ui::chrome::title_bar_height_pixels;
use crate::ui::window_manager::{Event as UiEvent, Repaint as UiRepaintEvent};
use anyhow::{Context, Result, anyhow, bail};
pub(crate) use connection::{ConnectingWindowsRunConfig, ConnectingWindowsRunner};
pub(in crate::application::viewer) use connection::{
    WindowsConnectionApp, run, run_connecting, ui_frame_interval,
};
pub(in crate::application::viewer) use player::ThreadedWindowsApp;
pub(in crate::application::viewer) use shader::{VideoShaderRenderer, VideoTextureView};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, Instant};
pub(in crate::application::viewer) use swapchain::fit_rect;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowAttributes, WindowId};

mod chrome;
mod connection;
mod player;
mod renderer;
mod worker;

mod plugin_capture;
mod plugin_video;
mod screen_windows;
