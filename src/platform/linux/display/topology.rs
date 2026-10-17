//! X11 display inventory through RandR. An output's connector name is its
//! identity; protocol screen IDs and CRTC numbers never stand in for it.
//!
//! The saved and applied forms carry the same meaning as on Windows: a
//! topology is the complete set of active outputs, each with a position, a
//! mode and a rotation. Rotation is kept as the RandR bitmask and refresh as
//! the mode's dot clock over its frame length, which is all equality needs.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::{
    self, ConnectionExt as _, GetCrtcInfoReply, GetOutputInfoReply, ModeFlag, ModeInfo, Rotation,
};
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

/// A display mode the server refused to apply.
///
/// Windows tests a mode with the driver before changing anything and reports
/// a refusal with this. Xwayland rejects mode changes outright, so it reports
/// the same value: the connection keeps the current mode instead of failing.
#[derive(Debug)]
pub(crate) struct ModeTestRejected;
impl std::fmt::Display for ModeTestRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("显示器拒绝该模式")
    }
}
impl std::error::Error for ModeTestRejected {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Mode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dpi {
    pub current: u32,
    pub recommended: u32,
    pub supported: Vec<u32>,
}
/// Which virtual-display driver owns a target. X11 has neither the UU nor the
/// OpenUUYC virtual adapter, so Linux targets never carry one; shared code
/// only compares against the variants.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum VirtualProvider {
    Uu,
    OpenUuyc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub identity: String,
    pub name: String,
    pub source: String,
    pub adapter: u64,
    pub adapter_path: String,
    pub target: u32,
    pub source_adapter: u64,
    pub source_id: u32,
    pub active: bool,
    pub available: bool,
    pub virtual_provider: Option<VirtualProvider>,
    pub modes: Vec<Mode>,
    /// The output's preferred mode (RandR lists it first among `num_preferred`),
    /// oriented like the active desktop; only for active outputs, as on Windows.
    pub preferred_mode: Option<(u32, u32)>,
    /// X11 has no per-output scale; desktops that scale do it for the whole
    /// session, so no output offers a DPI choice.
    pub dpi: Option<Dpi>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedTarget {
    pub identity: String,
    pub source_group: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub rotation: i32,
    pub refresh_numerator: u32,
    pub refresh_denominator: u32,
    pub scan_line_ordering: i32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedTopology {
    pub targets: Vec<SavedTarget>,
}

/// An active output as the capture sees it.
#[derive(Clone, Debug)]
pub(crate) struct Monitor {
    pub identity: String,
    pub name: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub hz: Option<u32>,
    pub primary: bool,
}

pub(crate) fn identity(name: &str) -> String {
    format!("x11/{name}")
}

/// [`identity`] 的逆：合成器和虚拟屏辅助程序认的是输出名本身（X11/DRM 的连接器名），
/// 只有屏幕清单和日志里才用带前缀的 identity。混了这两套说法就会得到
/// 「明明建出来了却说没出现在桌面里」这种结果。
pub(crate) fn output_name(identity: &str) -> &str {
    identity.strip_prefix("x11/").unwrap_or(identity)
}

#[cfg(test)]
mod tests {
    use super::{identity, output_name};

    #[test]
    fn an_identity_can_be_turned_back_into_the_output_name() {
        assert_eq!(output_name(&identity("DP-3")), "DP-3");
        assert_eq!(output_name(&identity("Virtual-1")), "Virtual-1");
        // 已经是不带前缀的名字时原样返回，免得把别的写法弄坏。
        assert_eq!(output_name("eDP-1"), "eDP-1");
    }
}

struct Output {
    id: randr::Output,
    name: String,
    info: GetOutputInfoReply,
    crtc: Option<GetCrtcInfoReply>,
}
impl Output {
    fn identity(&self) -> String {
        identity(&self.name)
    }
    fn connected(&self) -> bool {
        self.info.connection == randr::Connection::CONNECTED
    }
    /// Driving a mode, as opposed to connected but switched off.
    fn active(&self) -> Option<&GetCrtcInfoReply> {
        self.crtc
            .as_ref()
            .filter(|crtc| crtc.mode != 0 && crtc.width > 0 && crtc.height > 0)
    }
}

/// One consistent read of the server's RandR state.
pub(crate) struct Topology {
    connection: RustConnection,
    root: Window,
    config_timestamp: u32,
    crtcs: Vec<randr::Crtc>,
    modes: Vec<ModeInfo>,
    outputs: Vec<Output>,
    primary: randr::Output,
    active_only: bool,
}

fn connect() -> Result<(RustConnection, Window)> {
    let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
    let root = connection.setup().roots[screen].root;
    let version = connection
        .randr_query_version(1, 5)?
        .reply()
        .context("X 服务器不支持 RandR")?;
    ensure!(
        (version.major_version, version.minor_version) >= (1, 3),
        "X 服务器的 RandR 版本过旧"
    );
    Ok((connection, root))
}

/// Refresh as the exact ratio of pixel clock to frame length.
fn refresh(mode: &ModeInfo) -> (u32, u32) {
    let mut numerator = u64::from(mode.dot_clock);
    let mut denominator = u64::from(mode.htotal) * u64::from(mode.vtotal);
    if mode.mode_flags & ModeFlag::DOUBLE_SCAN != ModeFlag::from(0u32) {
        denominator *= 2;
    }
    if mode.mode_flags & ModeFlag::INTERLACE != ModeFlag::from(0u32) {
        numerator *= 2;
    }
    // Both fit comfortably for any real mode; clamp rather than wrap.
    (
        numerator.min(u64::from(u32::MAX)) as u32,
        denominator.clamp(1, u64::from(u32::MAX)) as u32,
    )
}
fn hz(mode: &ModeInfo) -> Option<u32> {
    let (numerator, denominator) = refresh(mode);
    (numerator > 0).then(|| ((f64::from(numerator) / f64::from(denominator)).round()) as u32)
}
fn interlaced(mode: &ModeInfo) -> bool {
    mode.mode_flags & ModeFlag::INTERLACE != ModeFlag::from(0u32)
}
fn sideways(rotation: Rotation) -> bool {
    rotation & (Rotation::ROTATE90 | Rotation::ROTATE270) != Rotation::from(0u16)
}

impl Topology {
    /// Whether the X server is Xwayland rather than a real X server.
    ///
    /// Xwayland advertises itself through an extension of the same name. Its
    /// RandR is a shim over the Wayland compositor, whose modes cannot be
    /// changed from X, so callers must not treat a refused mode as fatal.
    pub(crate) fn xwayland(&self) -> bool {
        self.connection
            .query_extension(b"XWAYLAND")
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.present)
    }

    pub(crate) fn query(active_only: bool) -> Result<Self> {
        let (connection, root) = connect()?;
        let resources = connection
            .randr_get_screen_resources_current(root)?
            .reply()
            .context("读取 RandR 资源失败")?;
        let primary = connection
            .randr_get_output_primary(root)?
            .reply()
            .map_or(0, |reply| reply.output);
        let mut outputs = Vec::new();
        for &id in &resources.outputs {
            let info = connection
                .randr_get_output_info(id, resources.config_timestamp)?
                .reply()
                .context("读取显示输出失败")?;
            let crtc = if info.crtc == 0 {
                None
            } else {
                Some(
                    connection
                        .randr_get_crtc_info(info.crtc, resources.config_timestamp)?
                        .reply()
                        .context("读取显示控制器失败")?,
                )
            };
            outputs.push(Output {
                id,
                name: String::from_utf8_lossy(&info.name).into_owned(),
                info,
                crtc,
            });
        }
        Ok(Self {
            connection,
            root,
            config_timestamp: resources.config_timestamp,
            crtcs: resources.crtcs,
            modes: resources.modes,
            outputs,
            primary,
            active_only,
        })
    }
    fn mode(&self, id: randr::Mode) -> Option<&ModeInfo> {
        self.modes.iter().find(|mode| mode.id == id)
    }
    fn visible(&self) -> impl Iterator<Item = &Output> {
        self.outputs
            .iter()
            .filter(|output| output.connected() && (!self.active_only || output.active().is_some()))
    }
    pub(crate) fn snapshot(&self) -> Result<SavedTopology> {
        let mut result = SavedTopology::default();
        for output in &self.outputs {
            let Some(crtc) = output.active() else {
                continue;
            };
            let mode = self.mode(crtc.mode).context("活动显示输出缺少模式")?;
            let (refresh_numerator, refresh_denominator) = refresh(mode);
            result.targets.push(SavedTarget {
                identity: output.identity(),
                source_group: format!("crtc/{}", output.info.crtc),
                left: i32::from(crtc.x),
                top: i32::from(crtc.y),
                width: u32::from(crtc.width),
                height: u32::from(crtc.height),
                rotation: i32::from(u16::from(crtc.rotation)),
                refresh_numerator,
                refresh_denominator,
                scan_line_ordering: if interlaced(mode) { 2 } else { 1 },
            });
        }
        Ok(result)
    }
    /// The targets without the Windows split between cheap metadata and the
    /// mode list: RandR reports modes with the outputs at no extra cost.
    pub(crate) fn metadata(&self) -> Result<Vec<Target>> {
        self.targets()
    }
    pub(crate) fn targets(&self) -> Result<Vec<Target>> {
        Ok(self
            .visible()
            .map(|output| {
                let active = output.active();
                let swap = active.is_some_and(|crtc| sideways(crtc.rotation));
                let mut modes: Vec<Mode> = output
                    .info
                    .modes
                    .iter()
                    .filter_map(|&id| self.mode(id))
                    .filter(|mode| !interlaced(mode))
                    .map(|mode| {
                        let (width, height) = (u32::from(mode.width), u32::from(mode.height));
                        let (width, height) = if swap {
                            (height, width)
                        } else {
                            (width, height)
                        };
                        Mode {
                            width,
                            height,
                            hz: hz(mode).unwrap_or(60),
                        }
                    })
                    .collect();
                // One entry per size: the fastest refresh the output offers there.
                modes.sort_by(|a, b| (b.width, b.height, b.hz).cmp(&(a.width, a.height, a.hz)));
                modes.dedup_by(|a, b| (a.width, a.height) == (b.width, b.height));
                let preferred_mode = active
                    .filter(|_| output.info.num_preferred > 0)
                    .and_then(|_| output.info.modes.first())
                    .and_then(|&id| self.mode(id))
                    .map(|mode| {
                        let (width, height) = (u32::from(mode.width), u32::from(mode.height));
                        if swap {
                            (height, width)
                        } else {
                            (width, height)
                        }
                    });
                Target {
                    identity: output.identity(),
                    name: output.name.clone(),
                    source: output.name.clone(),
                    adapter: 0,
                    adapter_path: String::new(),
                    target: output.id,
                    source_adapter: 0,
                    source_id: output.info.crtc,
                    active: active.is_some(),
                    available: output.connected(),
                    virtual_provider: None,
                    modes,
                    preferred_mode,
                    dpi: None,
                }
            })
            .collect())
    }
    /// The active outputs, in the capture's terms.
    pub(crate) fn monitors(&self) -> Vec<Monitor> {
        self.outputs
            .iter()
            .filter_map(|output| {
                let crtc = output.active()?;
                Some(Monitor {
                    identity: output.identity(),
                    name: output.name.clone(),
                    left: i32::from(crtc.x),
                    top: i32::from(crtc.y),
                    width: u32::from(crtc.width),
                    height: u32::from(crtc.height),
                    hz: self.mode(crtc.mode).and_then(hz),
                    primary: output.id == self.primary,
                })
            })
            .collect()
    }
}

/// A CRTC setting to send: which outputs it drives, where, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Config {
    x: i16,
    y: i16,
    mode: randr::Mode,
    rotation: Rotation,
    outputs: Vec<randr::Output>,
}

impl SavedTopology {
    /// Make exactly these outputs active with these modes and positions.
    /// Outputs that are no longer connected are returned to the caller.
    pub(crate) fn apply(&self) -> Result<Vec<String>> {
        ensure!(
            !self.targets.is_empty() && self.targets.len() <= 64,
            "无效的目标显示布局"
        );
        let current = Topology::query(false)?;
        let mut missing = Vec::new();
        let mut plan: Vec<(randr::Crtc, Config)> = Vec::new();
        for saved in &self.targets {
            let Some(output) = current
                .outputs
                .iter()
                .find(|o| o.connected() && o.identity() == saved.identity)
            else {
                missing.push(saved.identity.clone());
                continue;
            };
            let rotation = Rotation::from(u16::try_from(saved.rotation).context("无效旋转")?);
            let (width, height) = if sideways(rotation) {
                (saved.height, saved.width)
            } else {
                (saved.width, saved.height)
            };
            let wanted =
                f64::from(saved.refresh_numerator) / f64::from(saved.refresh_denominator.max(1));
            let mode = output
                .info
                .modes
                .iter()
                .filter_map(|&id| current.mode(id))
                .filter(|mode| (u32::from(mode.width), u32::from(mode.height)) == (width, height))
                .filter(|mode| interlaced(mode) == (saved.scan_line_ordering == 2))
                .min_by(|a, b| {
                    let rate = |m: &ModeInfo| {
                        let (n, d) = refresh(m);
                        (f64::from(n) / f64::from(d) - wanted).abs()
                    };
                    rate(a).total_cmp(&rate(b))
                })
                .with_context(|| format!("{} 不支持 {width}x{height}", output.name))?;
            let crtc = match output.info.crtc {
                0 => current
                    .crtcs
                    .iter()
                    .copied()
                    .find(|crtc| {
                        output.info.crtcs.contains(crtc)
                            && !current.outputs.iter().any(|o| o.info.crtc == *crtc)
                            && !plan.iter().any(|(used, _)| used == crtc)
                    })
                    .with_context(|| format!("没有可驱动 {} 的显示控制器", output.name))?,
                crtc => crtc,
            };
            ensure!(
                !plan.iter().any(|(used, _)| *used == crtc),
                "两个显示输出共用同一显示控制器"
            );
            plan.push((
                crtc,
                Config {
                    x: i16::try_from(saved.left).context("显示位置超出 X11 范围")?,
                    y: i16::try_from(saved.top).context("显示位置超出 X11 范围")?,
                    mode: mode.id,
                    rotation,
                    outputs: vec![output.id],
                },
            ));
        }
        ensure!(!plan.is_empty(), "目标显示布局中的显示器都已断开");
        current.commit(&plan)?;
        Ok(missing)
    }
}

impl Topology {
    /// Send `plan` as the complete CRTC configuration: CRTCs it leaves out are
    /// switched off. The root window is resized around the result.
    fn commit(&self, plan: &[(randr::Crtc, Config)]) -> Result<()> {
        let size = |config: &Config| -> Result<(i32, i32)> {
            let mode = self.mode(config.mode).context("显示模式已失效")?;
            let (width, height) = (i32::from(mode.width), i32::from(mode.height));
            Ok(if sideways(config.rotation) {
                (height, width)
            } else {
                (width, height)
            })
        };
        let mut right = 0;
        let mut bottom = 0;
        for (_, config) in plan {
            let (width, height) = size(config)?;
            right = right.max(i32::from(config.x) + width);
            bottom = bottom.max(i32::from(config.y) + height);
        }
        let range = self
            .connection
            .randr_get_screen_size_range(self.root)?
            .reply()?;
        ensure!(
            (i32::from(range.min_width)..=i32::from(range.max_width)).contains(&right)
                && (i32::from(range.min_height)..=i32::from(range.max_height)).contains(&bottom),
            "显示布局超出 X 服务器支持的桌面尺寸"
        );
        let geometry = self.connection.get_geometry(self.root)?.reply()?;
        let screen = self
            .connection
            .setup()
            .roots
            .iter()
            .find(|screen| screen.root == self.root)
            .context("X 屏幕已失效")?;
        // Keep the reported physical DPI where the new size puts it.
        let millimetres = |pixels: i32, old_pixels: u16, old_mm: u16| -> u32 {
            if old_pixels == 0 || old_mm == 0 {
                (f64::from(pixels) * 25.4 / 96.0).round() as u32
            } else {
                (f64::from(pixels) * f64::from(old_mm) / f64::from(old_pixels)).round() as u32
            }
        };
        let current = |crtc: randr::Crtc| {
            self.outputs
                .iter()
                .filter_map(|o| o.active().filter(|_| o.info.crtc == crtc))
                .next()
        };
        let set = |crtc: randr::Crtc, config: Option<&Config>| -> Result<()> {
            let reply = match config {
                Some(c) => self.connection.randr_set_crtc_config(
                    crtc,
                    x11rb::CURRENT_TIME,
                    self.config_timestamp,
                    c.x,
                    c.y,
                    c.mode,
                    c.rotation,
                    &c.outputs,
                )?,
                None => self.connection.randr_set_crtc_config(
                    crtc,
                    x11rb::CURRENT_TIME,
                    self.config_timestamp,
                    0,
                    0,
                    0,
                    Rotation::ROTATE0,
                    &[],
                )?,
            }
            .reply()
            .context("设置显示控制器失败")?;
            if reply.status != randr::SetConfig::SUCCESS {
                bail!("X 服务器拒绝显示配置：{:?}", reply.status);
            }
            Ok(())
        };
        self.connection.grab_server()?;
        let result = (|| -> Result<()> {
            // Off first: CRTCs leaving the layout, and changed CRTCs whose
            // present rectangle would not fit the new root size.
            for &crtc in &self.crtcs {
                let Some(active) = current(crtc) else {
                    continue;
                };
                let wanted = plan
                    .iter()
                    .find(|(c, _)| *c == crtc)
                    .map(|(_, config)| config);
                let fits = i32::from(active.x) + i32::from(active.width) <= right
                    && i32::from(active.y) + i32::from(active.height) <= bottom;
                let unchanged = wanted.is_some_and(|w| {
                    (w.x, w.y, w.mode, w.rotation)
                        == (active.x, active.y, active.mode, active.rotation)
                        && w.outputs == active.outputs
                });
                if wanted.is_none() || (!unchanged && !fits) {
                    set(crtc, None)?;
                }
            }
            self.connection
                .randr_set_screen_size(
                    self.root,
                    u16::try_from(right)?,
                    u16::try_from(bottom)?,
                    millimetres(right, geometry.width, screen.width_in_millimeters),
                    millimetres(bottom, geometry.height, screen.height_in_millimeters),
                )?
                .check()
                .context("调整桌面尺寸失败")?;
            for (crtc, config) in plan {
                let unchanged = current(*crtc).is_some_and(|active| {
                    (config.x, config.y, config.mode, config.rotation)
                        == (active.x, active.y, active.mode, active.rotation)
                        && config.outputs == active.outputs
                });
                if !unchanged {
                    set(*crtc, Some(config))?;
                }
            }
            Ok(())
        })();
        let _ = self.connection.ungrab_server();
        let _ = self.connection.flush();
        result
    }
}

impl Target {
    pub(crate) fn revalidate(&self) -> Result<Self> {
        Topology::query(false)?
            .targets()?
            .into_iter()
            .find(|t| t.identity == self.identity && t.available)
            .context("目标显示器已断开")
    }
    pub(crate) fn set_dpi(&self, _requested: u32, _active: impl Fn() -> bool) -> Result<()> {
        bail!("X11 不支持按显示器调整缩放")
    }
    pub(crate) fn set_resolution(
        &self,
        width: u32,
        height: u32,
        active: impl Fn() -> bool,
    ) -> Result<()> {
        let target = self.revalidate()?;
        ensure!(target.active, "目标显示器未启用");
        // Xwayland presents RandR as a shim over the Wayland compositor, which
        // owns the real modes; switching one there always fails with BadMatch
        // and can leave CRTCs switched off. Treat the mode as refused so the
        // connection keeps the current one, which is what the controller gets
        // anyway on a compositor that will not resize.
        if Topology::query(false)?.xwayland() {
            return Err(ModeTestRejected.into());
        }
        ensure!(
            target
                .modes
                .iter()
                .any(|m| m.width == width && m.height == height),
            "不支持的物理分辨率"
        );
        let mut layout = Topology::query(true)?.snapshot()?;
        let index = layout
            .targets
            .iter()
            .position(|t| t.identity == self.identity)
            .context("目标显示器未启用")?;
        let old = layout.targets[index].clone();
        if (old.width, old.height) == (width, height) {
            return Ok(());
        }
        // Outputs lying wholly beyond the resized one's right or bottom edge
        // move with it, so the desktop neither overlaps nor gains a gap.
        let dx = i64::from(width) - i64::from(old.width);
        let dy = i64::from(height) - i64::from(old.height);
        let old_right = i64::from(old.left) + i64::from(old.width);
        let old_bottom = i64::from(old.top) + i64::from(old.height);
        for (i, other) in layout.targets.iter_mut().enumerate() {
            if i == index {
                continue;
            }
            if i64::from(other.left) >= old_right {
                other.left = i32::try_from(i64::from(other.left) + dx)?;
            }
            if i64::from(other.top) >= old_bottom {
                other.top = i32::try_from(i64::from(other.top) + dy)?;
            }
        }
        layout.targets[index].width = width;
        layout.targets[index].height = height;
        // Apply once; an ambiguous failure is never automatically replayed.
        ensure!(active(), "显示操作已取消");
        let missing = layout.apply()?;
        ensure!(missing.is_empty(), "显示器在切换分辨率时断开");
        Ok(())
    }
}
