//! Windows display inventory. Protocol screen IDs and virtual-device ownership
//! belong to the host; Windows source/target IDs never stand in for either.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::mem::size_of;
use windows::{
    Win32::{
        Devices::Display::*,
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LUID},
        Graphics::Gdi::*,
    },
    core::PCWSTR,
};

/// CDS_TEST rejected the requested mode before any change was applied. Initial
/// preferences may retain the current mode; explicit runtime requests still fail.
#[derive(Debug, thiserror::Error)]
#[error("显示驱动拒绝分辨率：{0}")]
pub(crate) struct ModeTestRejected(pub i32);

pub(crate) const DPI_VALUES: [u32; 12] =
    [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VirtualProvider {
    /// Another product's virtual display adapter (GameViewer, OpenUUYC).
    /// Never owned, managed or counted as a real replacement display.
    Foreign,
    /// This client's own Farwing Display adapter.
    Farwing,
}

impl VirtualProvider {
    fn from_adapter_name(name: &str) -> Option<Self> {
        match name {
            "GameViewer Virtual Display Adapter" | "OpenUUYC Virtual Display Adapter" => {
                Some(Self::Foreign)
            }
            "Farwing Virtual Display Adapter" => Some(Self::Farwing),
            _ => None,
        }
    }
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
    /// Both providers use the same virtual-screen wire type. Native
    /// operations and session cleanup still verify the actual driver/owner.
    pub virtual_provider: Option<VirtualProvider>,
    pub modes: Vec<Mode>,
    /// Driver-reported monitor preferred mode, oriented like the active desktop.
    /// Independent of the current source mode and supported-mode maximum.
    pub preferred_mode: Option<(u32, u32)>,
    pub dpi: Option<Dpi>,
}
pub(crate) struct Topology {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

/// Reading monitor names and every display mode is slow (one driver call per
/// mode), and the host reads its display inventory every second. While the
/// display configuration (`QueryDisplayConfig`, cheap) is unchanged, those
/// results are reused for at most this long.
const INVENTORY_REUSE: std::time::Duration = std::time::Duration::from_secs(10);

struct Reused<T> {
    signature: u64,
    at: std::time::Instant,
    value: T,
}
static METADATA_REUSE: std::sync::Mutex<Vec<Reused<Vec<Target>>>> =
    std::sync::Mutex::new(Vec::new());
static MODES_REUSE: std::sync::Mutex<Vec<Reused<(String, Vec<Mode>)>>> =
    std::sync::Mutex::new(Vec::new());

/// The cached value for `signature` that `matches`, if read within
/// `INVENTORY_REUSE`; otherwise `read` it and keep it.
fn reuse<T: Clone>(
    cache: &std::sync::Mutex<Vec<Reused<T>>>,
    signature: u64,
    now: std::time::Instant,
    matches: impl Fn(&T) -> bool,
    read: impl FnOnce() -> Result<T>,
) -> Result<T> {
    {
        let mut entries = cache.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|entry| now.saturating_duration_since(entry.at) < INVENTORY_REUSE);
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.signature == signature && matches(&entry.value))
        {
            return Ok(entry.value.clone());
        }
    }
    let value = read()?;
    let mut entries = cache.lock().unwrap_or_else(|e| e.into_inner());
    entries.retain(|entry| !(entry.signature == signature && matches(&entry.value)));
    if entries.len() >= 8 {
        entries.remove(0);
    }
    entries.push(Reused {
        signature,
        at: now,
        value: value.clone(),
    });
    Ok(value)
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

pub(crate) fn luid(value: LUID) -> u64 {
    (u64::from(value.HighPart as u32) << 32) | u64::from(value.LowPart)
}
pub(crate) fn native_luid(value: u64) -> LUID {
    LUID {
        LowPart: value as u32,
        HighPart: (value >> 32) as i32,
    }
}
fn text(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|c| *c == 0).unwrap_or(value.len())])
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn virtual_sources() -> Vec<(String, VirtualProvider)> {
    // Identify only the known adapters by name, not EDID model names, arbitrary
    // "Virtual" names or other indirect-display providers.
    let mut sources = Vec::new();
    for index in 0.. {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe { EnumDisplayDevicesW(None, index, &mut device, 0) }.as_bool() {
            break;
        }
        if let Some(provider) = VirtualProvider::from_adapter_name(&text(&device.DeviceString)) {
            sources.push((text(&device.DeviceName), provider));
        }
    }
    sources
}

/// The monitor device path segment of the Farwing Display EDID (`FWG0001`).
const FARWING_MONITOR: &str = "#fwg0001#";

impl Topology {
    /// The monitor identity of a target the display driver reported. Windows
    /// may present an indirect display through its render adapter, under
    /// another adapter and target ID; every path has the same identity.
    pub(crate) fn identity_of(&self, adapter: u64, target: u32) -> Option<String> {
        self.paths
            .iter()
            .filter(|p| luid(p.targetInfo.adapterId) == adapter && p.targetInfo.id == target)
            .find_map(|p| target_identity(p).ok())
    }

    pub(crate) fn snapshot(&self) -> Result<SavedTopology> {
        let mut result = SavedTopology::default();
        for path in &self.paths {
            if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0
                || !path.targetInfo.targetAvailable.as_bool()
            {
                continue;
            }
            let identity = target_identity(path)?;
            let mode = self
                .modes
                .get(unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize)
                .context("活动显示路径缺少源模式")?;
            ensure!(
                mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
                "无效源模式类型"
            );
            let source = unsafe { mode.Anonymous.sourceMode };
            let scan = self
                .modes
                .get(unsafe { path.targetInfo.Anonymous.modeInfoIdx } as usize)
                .filter(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET)
                .map_or(path.targetInfo.scanLineOrdering.0, |m| unsafe {
                    m.Anonymous
                        .targetMode
                        .targetVideoSignalInfo
                        .scanLineOrdering
                        .0
                });
            ensure!((1..=3).contains(&scan), "活动显示模式缺少有效扫描方式");
            result.targets.push(SavedTarget {
                identity,
                source_group: format!(
                    "{:016X}/{}",
                    luid(path.sourceInfo.adapterId),
                    path.sourceInfo.id
                ),
                left: source.position.x,
                top: source.position.y,
                width: source.width,
                height: source.height,
                rotation: path.targetInfo.rotation.0,
                refresh_numerator: path.targetInfo.refreshRate.Numerator,
                refresh_denominator: path.targetInfo.refreshRate.Denominator,
                scan_line_ordering: scan,
            });
        }
        Ok(result)
    }
    pub(crate) fn query(active_only: bool) -> Result<Self> {
        let flags = if active_only {
            QDC_ONLY_ACTIVE_PATHS
        } else {
            QDC_ALL_PATHS
        };
        for _ in 0..3 {
            let (mut paths, mut modes) = (0, 0);
            let status = unsafe { GetDisplayConfigBufferSizes(flags, &mut paths, &mut modes) };
            ensure!(
                status == ERROR_SUCCESS,
                "读取显示拓扑大小失败：{}",
                status.0
            );
            ensure!(paths <= 4096 && modes <= 16384, "显示拓扑规模异常");
            let mut result = Self {
                paths: vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize],
                modes: vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize],
            };
            let status = unsafe {
                QueryDisplayConfig(
                    flags,
                    &mut paths,
                    result.paths.as_mut_ptr(),
                    &mut modes,
                    result.modes.as_mut_ptr(),
                    None,
                )
            };
            if status == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            ensure!(status == ERROR_SUCCESS, "读取显示拓扑失败：{}", status.0);
            result.paths.truncate(paths as usize);
            result.modes.truncate(modes as usize);
            return Ok(result);
        }
        bail!("显示拓扑正在变化，请稍后重试")
    }

    /// Identifies this display configuration: paths, sources, targets, their
    /// modes and availability.
    fn signature(&self) -> u64 {
        use std::hash::Hasher;
        fn bytes<T>(items: &[T]) -> &[u8] {
            // SAFETY: plain Win32 structs written by QueryDisplayConfig (or
            // zero-initialized); their bytes are only hashed.
            unsafe {
                std::slice::from_raw_parts(
                    items.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(items),
                )
            }
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        hasher.write(bytes(&self.paths));
        hasher.write(bytes(&self.modes));
        hasher.finish()
    }

    pub(crate) fn targets(&self) -> Result<Vec<Target>> {
        let signature = self.signature();
        let now = std::time::Instant::now();
        let mut targets = self.metadata()?;
        for target in &mut targets {
            if !target.source.is_empty() {
                let source = target.source.clone();
                target.modes = reuse(
                    &MODES_REUSE,
                    signature,
                    now,
                    |(cached, _)| *cached == source,
                    || Ok((source.clone(), modes(&source)?)),
                )?
                .1;
            }
        }
        Ok(targets)
    }

    pub(crate) fn metadata(&self) -> Result<Vec<Target>> {
        let mut targets = reuse(
            &METADATA_REUSE,
            self.signature(),
            std::time::Instant::now(),
            |_| true,
            || self.read_metadata(),
        )?;
        // The scale can change without the configuration: read it each time.
        for target in &mut targets {
            if target.active {
                target.dpi = dpi(native_luid(target.source_adapter), target.source_id).ok();
            }
        }
        Ok(targets)
    }

    fn read_metadata(&self) -> Result<Vec<Target>> {
        let mut targets: Vec<Target> = Vec::new();
        let virtual_sources = virtual_sources();
        for path in &self.paths {
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            let status = unsafe { DisplayConfigGetDeviceInfo(&mut target.header) };
            if status != 0 {
                // Inactive QDC_ALL_PATHS candidates can have no monitor attached.
                ensure!(
                    path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0,
                    "读取活动显示器身份失败：{status}"
                );
                continue;
            }
            let identity = text(&target.monitorDevicePath).to_ascii_lowercase();
            if identity.is_empty() {
                continue;
            }
            let active = path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0;
            let available = path.targetInfo.targetAvailable.as_bool();
            if let Some(previous) = targets.iter().position(|t| t.identity == identity) {
                // One monitor, several paths: keep the active, else an available one.
                if (active, available) <= (targets[previous].active, targets[previous].available) {
                    continue;
                }
                targets.remove(previous);
            }
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            let source = if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } == 0 {
                text(&source.viewGdiDeviceName)
            } else {
                String::new()
            };
            let mut adapter = DISPLAYCONFIG_ADAPTER_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
                    size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: 0,
                },
                ..Default::default()
            };
            let adapter_path = if unsafe { DisplayConfigGetDeviceInfo(&mut adapter.header) } == 0 {
                text(&adapter.adapterDevicePath).to_ascii_lowercase()
            } else {
                String::new()
            };
            let virtual_provider = virtual_sources
                .iter()
                .find(|(name, _)| name == &source)
                .map(|(_, provider)| provider.clone())
                // Windows may present a Farwing monitor through its render
                // adapter; its project-local EDID ID still names the driver.
                .or_else(|| {
                    identity
                        .contains(FARWING_MONITOR)
                        .then_some(VirtualProvider::Farwing)
                });
            targets.push(Target {
                identity,
                name: text(&target.monitorFriendlyDeviceName),
                modes: Vec::new(),
                preferred_mode: active.then(|| preferred_mode(path)).flatten(),
                dpi: if active {
                    dpi(path.sourceInfo.adapterId, path.sourceInfo.id).ok()
                } else {
                    None
                },
                source,
                adapter: luid(path.targetInfo.adapterId),
                adapter_path,
                target: path.targetInfo.id,
                source_adapter: luid(path.sourceInfo.adapterId),
                source_id: path.sourceInfo.id,
                active,
                available,
                virtual_provider,
            });
        }
        Ok(targets)
    }
}

fn preferred_mode(path: &DISPLAYCONFIG_PATH_INFO) -> Option<(u32, u32)> {
    let mut preferred = DISPLAYCONFIG_TARGET_PREFERRED_MODE {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE,
            size: size_of::<DISPLAYCONFIG_TARGET_PREFERRED_MODE>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut preferred.header) } != 0
        || !(1..=16384).contains(&preferred.width)
        || !(1..=16384).contains(&preferred.height)
    {
        return None;
    }
    Some(
        if matches!(
            path.targetInfo.rotation,
            DISPLAYCONFIG_ROTATION_ROTATE90 | DISPLAYCONFIG_ROTATION_ROTATE270
        ) {
            (preferred.height, preferred.width)
        } else {
            (preferred.width, preferred.height)
        },
    )
}

fn target_identity(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String> {
    let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut target.header) };
    ensure!(status == 0, "读取显示目标身份失败：{status}");
    let identity = text(&target.monitorDevicePath).to_ascii_lowercase();
    ensure!(!identity.is_empty(), "显示目标身份为空");
    Ok(identity)
}

impl SavedTopology {
    /// Rebind saved monitor identities to current Windows IDs. Removed monitors
    /// are returned to the caller; their old integer target IDs are never reused.
    pub(crate) fn apply(&self) -> Result<Vec<String>> {
        use std::collections::{HashMap, HashSet};
        ensure!(
            !self.targets.is_empty() && self.targets.len() <= 64,
            "无效的目标显示布局"
        );
        let inventory = Topology::query(false)?;
        let current = Topology::query(true)?;
        let mut candidates = Vec::new();
        for path in current.paths.iter().copied().chain(inventory.paths) {
            if path.targetInfo.targetAvailable.as_bool() {
                if let Ok(identity) = target_identity(&path) {
                    candidates.push((identity, path));
                }
            }
        }
        let mut paths = Vec::new();
        let mut modes = Vec::new();
        let mut groups = HashMap::new();
        let mut used_sources = HashSet::new();
        let mut seen_targets = HashSet::new();
        let mut missing = Vec::new();
        for saved in &self.targets {
            ensure!(seen_targets.insert(&saved.identity), "布局包含重复显示目标");
            ensure!(
                (1..=16384).contains(&saved.width)
                    && (1..=16384).contains(&saved.height)
                    && (1..=4).contains(&saved.rotation)
                    && (1..=3).contains(&saved.scan_line_ordering)
                    && saved.refresh_denominator != 0,
                "无效的保存显示模式"
            );
            let group = groups.get(&saved.source_group).copied();
            let candidate = candidates
                .iter()
                .filter(|(identity, _)| identity == &saved.identity)
                .filter(|(_, p)| {
                    let key = (luid(p.sourceInfo.adapterId), p.sourceInfo.id);
                    group.map_or(!used_sources.contains(&key), |(g, _)| g == key)
                })
                .min_by_key(|(_, p)| p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0);
            let Some((_, candidate)) = candidate else {
                missing.push(saved.identity.clone());
                continue;
            };
            let mut path = *candidate;
            let source_key = (luid(path.sourceInfo.adapterId), path.sourceInfo.id);
            let index = if let Some((_, index)) = group {
                index
            } else {
                let index = modes.len() as u32;
                modes.push(DISPLAYCONFIG_MODE_INFO {
                    infoType: DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
                    id: path.sourceInfo.id,
                    adapterId: path.sourceInfo.adapterId,
                    Anonymous: DISPLAYCONFIG_MODE_INFO_0 {
                        sourceMode: DISPLAYCONFIG_SOURCE_MODE {
                            width: saved.width,
                            height: saved.height,
                            pixelFormat: DISPLAYCONFIG_PIXELFORMAT_32BPP,
                            position: windows::Win32::Foundation::POINTL {
                                x: saved.left,
                                y: saved.top,
                            },
                        },
                    },
                });
                groups.insert(saved.source_group.clone(), (source_key, index));
                used_sources.insert(source_key);
                index
            };
            path.flags = DISPLAYCONFIG_PATH_ACTIVE;
            path.sourceInfo.Anonymous.modeInfoIdx = index;
            path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
            // Preserve complete timings for unchanged active targets. Inactive
            // possibilities returned by QDC_ALL_PATHS are not a substitute for
            // the active path's target-mode/scan/scaling metadata.
            if let Some(active) = current.paths.iter().find(|active| {
                active.targetInfo.adapterId == path.targetInfo.adapterId
                    && active.targetInfo.id == path.targetInfo.id
                    && active.sourceInfo.adapterId == path.sourceInfo.adapterId
                    && active.sourceInfo.id == path.sourceInfo.id
            }) {
                path.targetInfo.scaling = active.targetInfo.scaling;
                path.targetInfo.scanLineOrdering = active.targetInfo.scanLineOrdering;
                if let Some(source) = current
                    .modes
                    .get(unsafe { active.sourceInfo.Anonymous.modeInfoIdx } as usize)
                    && source.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                {
                    let source = unsafe { source.Anonymous.sourceMode };
                    let rate = active.targetInfo.refreshRate;
                    if source.width == saved.width
                        && source.height == saved.height
                        && active.targetInfo.rotation.0 == saved.rotation
                        && u64::from(rate.Numerator) * u64::from(saved.refresh_denominator)
                            == u64::from(saved.refresh_numerator) * u64::from(rate.Denominator)
                    {
                        if let Some(target) = current
                            .modes
                            .get(unsafe { active.targetInfo.Anonymous.modeInfoIdx } as usize)
                            && target.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
                        {
                            path.targetInfo.Anonymous.modeInfoIdx = modes.len() as u32;
                            modes.push(*target);
                        }
                    }
                }
            }
            if path.targetInfo.scaling.0 == 0 {
                path.targetInfo.scaling = DISPLAYCONFIG_SCALING_PREFERRED;
            }
            path.targetInfo.rotation = DISPLAYCONFIG_ROTATION(saved.rotation);
            path.targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL {
                Numerator: saved.refresh_numerator,
                Denominator: saved.refresh_denominator,
            };
            // UNSPECIFIED is valid only together with a zero/zero refresh rate.
            // Restoring an explicit refresh rate also restores its scan order.
            path.targetInfo.scanLineOrdering =
                DISPLAYCONFIG_SCANLINE_ORDERING(saved.scan_line_ordering);
            paths.push(path);
        }
        ensure!(!paths.is_empty(), "原显示目标均不可用，保留当前布局");
        let flags = SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
        let status = unsafe { SetDisplayConfig(Some(&paths), Some(&modes), flags | SDC_VALIDATE) };
        ensure!(status == 0, "显示驱动拒绝目标布局：{}", status);
        let status = unsafe {
            SetDisplayConfig(
                Some(&paths),
                Some(&modes),
                flags | SDC_APPLY | SDC_SAVE_TO_DATABASE,
            )
        };
        ensure!(status == 0, "应用显示布局失败：{}", status);
        Ok(missing)
    }
}

pub(crate) fn modes(source: &str) -> Result<Vec<Mode>> {
    let source = wide(source);
    let mut result = Vec::new();
    for index in 0..16384 {
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !unsafe {
            EnumDisplaySettingsExW(
                PCWSTR(source.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(index),
                &mut mode,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
        }
        .as_bool()
        {
            return Ok(result);
        }
        if mode.dmPelsWidth == 0 || mode.dmPelsHeight == 0 || mode.dmDisplayFrequency <= 1 {
            continue;
        }
        let mode = Mode {
            width: mode.dmPelsWidth,
            height: mode.dmPelsHeight,
            hz: mode.dmDisplayFrequency,
        };
        if !result.contains(&mode) {
            result.push(mode);
        }
    }
    bail!("显示模式列表过大")
}

#[repr(C)]
#[derive(Default)]
struct GetDpi {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    minimum: i32,
    current: i32,
    maximum: i32,
}
#[repr(C)]
#[derive(Default)]
struct SetDpi {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    relative: i32,
}

fn dpi_raw(adapter: LUID, id: u32) -> Result<GetDpi> {
    let mut value = GetDpi {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(-3),
            size: size_of::<GetDpi>() as u32,
            adapterId: adapter,
            id,
        },
        ..Default::default()
    };
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut value.header) };
    ensure!(status == 0, "读取DPI范围失败：{status}");
    ensure!(
        value.minimum <= 0
            && value.maximum >= 0
            && (value.minimum..=value.maximum).contains(&value.current),
        "无效DPI范围"
    );
    Ok(value)
}
fn dpi(adapter: LUID, id: u32) -> Result<Dpi> {
    let value = dpi_raw(adapter, id)?;
    let base = value.minimum.checked_neg().context("无效推荐DPI索引")?;
    let current = base.checked_add(value.current).context("无效当前DPI索引")?;
    let last = base.checked_add(value.maximum).context("无效最大DPI索引")?;
    let recommended = *DPI_VALUES.get(base as usize).context("未知推荐DPI")?;
    let current = *DPI_VALUES.get(current as usize).context("未知当前DPI")?;
    ensure!(last >= 0, "无效DPI上限");
    Ok(Dpi {
        current,
        recommended,
        supported: DPI_VALUES[..=(last as usize).min(DPI_VALUES.len() - 1)].to_vec(),
    })
}

impl Target {
    pub(crate) fn revalidate(&self) -> Result<Self> {
        Topology::query(false)?
            .metadata()?
            .into_iter()
            .find(|t| t.identity == self.identity && t.available)
            .context("目标显示器已断开")
    }
    pub(crate) fn set_dpi(&self, requested: u32, active: impl Fn() -> bool) -> Result<()> {
        let target = self.revalidate()?;
        ensure!(target.active, "目标显示器未启用");
        let adapter = native_luid(target.source_adapter);
        let value = dpi_raw(adapter, target.source_id)?;
        let index = DPI_VALUES
            .iter()
            .position(|v| *v == requested)
            .context("不支持的DPI比例")? as i32;
        let relative = index.checked_add(value.minimum).context("无效DPI偏移")?;
        ensure!(
            (value.minimum..=value.maximum).contains(&relative),
            "DPI超出显示器支持范围"
        );
        if value.current == relative {
            return Ok(());
        }
        let value = SetDpi {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(-4),
                size: size_of::<SetDpi>() as u32,
                adapterId: adapter,
                id: target.source_id,
            },
            relative,
        };
        ensure!(active(), "显示操作已取消");
        let status = unsafe { DisplayConfigSetDeviceInfo(&value.header) };
        ensure!(status == 0, "应用DPI失败：{status}");
        Ok(())
    }
    pub(crate) fn set_resolution(
        &self,
        width: u32,
        height: u32,
        active: impl Fn() -> bool,
    ) -> Result<()> {
        let target = self.revalidate()?;
        ensure!(target.active, "目标显示器未启用");
        let source = wide(&target.source);
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        ensure!(
            unsafe {
                EnumDisplaySettingsExW(
                    PCWSTR(source.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
            }
            .as_bool(),
            "读取当前显示模式失败"
        );
        if (mode.dmPelsWidth, mode.dmPelsHeight) == (width, height) {
            return Ok(());
        }
        ensure!(
            modes(&target.source)?
                .iter()
                .any(|m| m.width == width && m.height == height),
            "不支持的物理分辨率"
        );
        mode.dmPelsWidth = width;
        mode.dmPelsHeight = height;
        mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        let status = unsafe {
            ChangeDisplaySettingsExW(PCWSTR(source.as_ptr()), Some(&mode), None, CDS_TEST, None)
        };
        if status != DISP_CHANGE_SUCCESSFUL {
            return Err(ModeTestRejected(status.0).into());
        }
        // Apply once; an ambiguous failure is never automatically replayed.
        ensure!(active(), "显示操作已取消");
        let status = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(source.as_ptr()),
                Some(&mode),
                None,
                CDS_UPDATEREGISTRY,
                None,
            )
        };
        ensure!(
            status == DISP_CHANGE_SUCCESSFUL,
            "应用分辨率失败：{}",
            status.0
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn inventory_is_reused_while_the_configuration_is_unchanged() {
        let cache = std::sync::Mutex::new(Vec::new());
        let start = std::time::Instant::now();
        let mut reads = 0;
        let mut read = |value: u32| -> anyhow::Result<u32> {
            reads += 1;
            Ok(value)
        };
        assert_eq!(
            super::reuse(&cache, 1, start, |_| true, || read(10)).unwrap(),
            10
        );
        let later = start + std::time::Duration::from_secs(5);
        assert_eq!(
            super::reuse(&cache, 1, later, |_| true, || read(11)).unwrap(),
            10
        );
        // A changed configuration, or old results, are read again.
        assert_eq!(
            super::reuse(&cache, 2, later, |_| true, || read(12)).unwrap(),
            12
        );
        let expired = start + super::INVENTORY_REUSE;
        assert_eq!(
            super::reuse(&cache, 1, expired, |_| true, || read(13)).unwrap(),
            13
        );
        assert_eq!(reads, 3);
    }
}
