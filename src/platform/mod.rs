//! Native OS/driver implementations. Product policy belongs to sessions/features.
//! Select the real backend here; no placeholder implementations for future OSes.
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    audio_thread, capture, cursor_shape, decoder, device_profile, display, display_hdr, drag_drop,
    encoder, file_locations, graphics, host_service, input::deadline, loopback, notifications,
    power_request, surface, swapchain, system_power, terminal, transfer, video_shader,
    virtual_audio, wol,
};
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{
    audio_thread, capture, cursor_shape, deadline, decoder, device_profile, display, display_hdr,
    drag_drop, encoder, file_locations, graphics, host_service, loopback, notifications,
    power_request, surface, system_power, terminal, transfer, virtual_audio, virtual_display, wol,
};
// The pty half of the terminal is identical on both, so it lives in one place
// and each platform re-exports it. The rest of the macOS platform does not
// exist yet, which is why only `terminal` is selected here.
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod posix;
#[cfg(target_os = "macos")]
pub(crate) use macos::terminal;
/// 终端前台正在跑的命令。
///
/// 取的是前台进程组，不是 shell：用户在 shell 里敲的 `cargo build` 会让它
/// 成为前台组，shell 自己在前台时就是空闲。管道只报最后一个进程，和
/// `ps` 的 `tpgid` 看到的一致。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Foreground {
    /// 进程名（Linux 上是 `/proc/<pgid>/comm`）。
    pub name: String,
    /// 完整命令行，按 NUL 拆开拼回，过长会截断。
    pub command: String,
}

/// One output as the compositor lays it out, in the global space it resolves
/// an absolute pointer position in. X11 and Windows have no such thing: their
/// display rectangles are already the space a pointer is named in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompositorOutput {
    /// The connector name (`DP-1`), which is also the name XWayland gives the
    /// output. Missing where the compositor states none.
    pub name: Option<String>,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub rotation: Rotation,
}

/// How far the compositor has turned the panel, as `wl_output.transform`
/// states it. Only the touch path needs it: a touchscreen reports a position
/// on its own panel, which some compositors turn with the output and others
/// do not, while every other device names a position that is already a screen
/// position.
///
/// Only the Wayland layout reader builds one; Windows has no compositor
/// layout, so there the variants are never constructed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))]
pub(crate) enum Rotation {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

/// Every output the compositor lays out, named or not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CompositorLayout {
    pub outputs: Vec<CompositorOutput>,
}
impl CompositorLayout {
    /// The output a screen of this name is, in the compositor's space.
    pub fn output(&self, name: &str) -> Option<&CompositorOutput> {
        self.outputs
            .iter()
            .find(|output| output.name.as_deref() == Some(name))
    }
}

pub(crate) mod paths;
pub(crate) mod wallpaper;
