//! User-local player shortcuts. Hook reads are atomic and never perform file IO.
mod watcher;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
pub(crate) use watcher::Watcher;
#[cfg(windows)]
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0},
    System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
    UI::{
        Input::KeyboardAndMouse::{MAPVK_VSC_TO_VK_EX, MapVirtualKeyW},
        WindowsAndMessaging::GetForegroundWindow,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    ReleaseMouse,
    Fullscreen,
    Close,
    Performance,
}
impl Action {
    pub const ALL: [Self; 4] = [
        Self::ReleaseMouse,
        Self::Fullscreen,
        Self::Close,
        Self::Performance,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::ReleaseMouse => "退出键鼠控制",
            Self::Fullscreen => "切换全屏",
            Self::Close => "关闭串流窗口",
            Self::Performance => "切换性能监控",
        }
    }
    fn index(self) -> usize {
        self as usize
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub key: u16,
    pub modifiers: u8,
}
impl Binding {
    pub fn label(self) -> String {
        if self.key >= 96 && self.key <= 111 || self.key >= 186 {
            #[cfg(windows)]
            let name = {
                use windows::Win32::UI::Input::KeyboardAndMouse::{
                    GetKeyNameTextW, MAPVK_VK_TO_VSC,
                };
                let scan = unsafe { MapVirtualKeyW(u32::from(self.key), MAPVK_VK_TO_VSC) };
                let mut buffer = [0u16; 64];
                let count = unsafe { GetKeyNameTextW((scan << 16) as i32, &mut buffer) };
                (count > 0).then(|| String::from_utf16_lossy(&buffer[..count as usize]))
            };
            #[cfg(not(windows))]
            let name = crate::application::viewer::linux_keyboard::key_name(self.key)
                .map(ToOwned::to_owned);
            if let Some(name) = name {
                let mut names = Vec::new();
                for (bit, name) in [(1, "Ctrl"), (2, "Shift"), (4, "Alt"), (8, "Win")] {
                    if self.modifiers & bit != 0 {
                        names.push(name.to_owned());
                    }
                }
                names.push(name);
                return names.join(" + ");
            }
        }
        crate::plugins::hotkeys::label(&crate::plugins::hotkeys::Binding {
            key: self.key,
            modifiers: self.modifiers,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    schema: u8,
    pub bindings: [Option<Binding>; 4],
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: 1,
            bindings: [90, 70, 81, 83].map(|key| Some(Binding { key, modifiers: 7 })),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "快捷键配置格式不支持");
        for (index, binding) in self.bindings.iter().enumerate() {
            if let Some(b) = binding {
                ensure!(
                    (8..=254).contains(&b.key)
                        && !matches!(b.key,16..=18|91..=92|160..=165|229|231)
                        && b.modifiers <= 15,
                    "请选择一个普通按键或组合键"
                );
                ensure!(
                    !(b.key == 27 && b.modifiers == 0),
                    "Esc 用于取消与关闭菜单，请添加修饰键"
                );
                ensure!(
                    !(b.key == 46 && b.modifiers & 5 == 5)
                        && !(b.key == 76 && b.modifiers & 8 != 0),
                    "此组合由 Windows 保留"
                );
                if let Some(other) = self.bindings[..index].iter().position(|v| v == binding) {
                    bail!(
                        "与“{}”重复，请修改或解绑其中一项",
                        Action::ALL[other].label()
                    );
                }
            }
        }
        Ok(())
    }
    fn packed(&self) -> u64 {
        self.bindings.iter().enumerate().fold(0, |value, (i, b)| {
            value | (b.map_or(0, |b| u64::from(b.key) | (u64::from(b.modifiers) << 8)) << (i * 16))
        })
    }
    fn unpack(packed: u64) -> Self {
        Self {
            schema: 1,
            bindings: std::array::from_fn(|i| {
                let word = (packed >> (i * 16)) as u16;
                (word != 0).then_some(Binding {
                    key: word & 255,
                    modifiers: (word >> 8) as u8,
                })
            }),
        }
    }
}
struct Cache {
    revision: u64,
    checked: Option<Instant>,
    bytes: Option<Vec<u8>>,
    error: Option<String>,
}
struct Runtime {
    active: AtomicU64,
    capture: AtomicU64,
    typing: AtomicU64,
    cache: Mutex<Cache>,
}
fn runtime() -> &'static Runtime {
    static R: OnceLock<Runtime> = OnceLock::new();
    R.get_or_init(|| Runtime {
        active: AtomicU64::new(Settings::default().packed()),
        capture: AtomicU64::new(0),
        typing: AtomicU64::new(0),
        cache: Mutex::new(Cache {
            revision: 0,
            checked: None,
            bytes: None,
            error: None,
        }),
    })
}
pub(crate) fn revision() -> u64 {
    runtime().active.load(Ordering::Acquire)
}
pub(crate) fn match_key(key: u16, modifiers: u8) -> Option<Action> {
    let s = Settings::unpack(revision());
    Action::ALL
        .into_iter()
        .find(|a| s.bindings[a.index()] == Some(Binding { key, modifiers }))
}
pub(crate) fn label(action: Action) -> String {
    Settings::unpack(revision()).bindings[action.index()]
        .map_or_else(|| "未绑定".into(), Binding::label)
}
pub(crate) fn capture_active() -> bool {
    let owner = runtime().capture.load(Ordering::Acquire);
    owner != 0 && owner == foreground_window()
}

fn foreground_window() -> u64 {
    #[cfg(windows)]
    {
        unsafe { GetForegroundWindow().0 as u64 }
    }
    #[cfg(not(windows))]
    {
        focused_window()
    }
}
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "Only the Windows keyboard hook tracks a typing owner."
    )
)]
pub(crate) fn set_text_owner(owner: u64, active: bool) {
    if active {
        runtime().typing.store(owner, Ordering::Release);
    } else {
        let _ = runtime()
            .typing
            .compare_exchange(owner, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}
pub(crate) fn suspended() -> bool {
    capture_active() || {
        let owner = runtime().typing.load(Ordering::Acquire);
        // A text field claims the shortcuts while its own window has focus.
        #[cfg(windows)]
        let focused = unsafe { GetForegroundWindow().0 as u64 };
        #[cfg(not(windows))]
        let focused = focused_window();
        owner != 0 && owner == focused
    }
}

/// The player publishes its focused window id here; X11 and Wayland do not let
/// a process ask which window the compositor considers focused.
#[cfg(not(windows))]
static FOCUSED: AtomicU64 = AtomicU64::new(0);

#[cfg(not(windows))]
pub(crate) fn set_focused_window(owner: u64) {
    FOCUSED.store(owner, Ordering::Release);
}

#[cfg(not(windows))]
pub(crate) fn focused_window() -> u64 {
    FOCUSED.load(Ordering::Acquire)
}
pub(crate) fn physical_key(key: winit::keyboard::PhysicalKey) -> Option<u16> {
    use winit::keyboard::{KeyCode, PhysicalKey};
    #[cfg(windows)]
    use winit::platform::scancode::PhysicalKeyExtScancode;
    let keypad = match key {
        PhysicalKey::Code(k) => match k {
            KeyCode::Numpad0 => Some(96),
            KeyCode::Numpad1 => Some(97),
            KeyCode::Numpad2 => Some(98),
            KeyCode::Numpad3 => Some(99),
            KeyCode::Numpad4 => Some(100),
            KeyCode::Numpad5 => Some(101),
            KeyCode::Numpad6 => Some(102),
            KeyCode::Numpad7 => Some(103),
            KeyCode::Numpad8 => Some(104),
            KeyCode::Numpad9 => Some(105),
            KeyCode::NumpadDecimal => Some(110),
            _ => None,
        },
        _ => None,
    };
    #[cfg(windows)]
    if let Some(vk) = keypad {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyState};
        if unsafe { (GetKeyState(144) & 1 != 0) ^ (GetAsyncKeyState(16) < 0) } {
            return Some(vk);
        }
    }
    // winit already reports the numeric keypad as its own physical keys, so the
    // NumLock/Shift dance the Windows scancode path needs does not apply here.
    #[cfg(not(windows))]
    if let Some(vk) = keypad {
        return Some(vk);
    }
    #[cfg(windows)]
    {
        key.to_scancode()
            .map(|scan| unsafe { MapVirtualKeyW(scan, MAPVK_VSC_TO_VK_EX) as u16 })
            .filter(|key| *key != 0)
    }
    #[cfg(not(windows))]
    {
        crate::application::viewer::linux_keyboard::physical_virtual_key(key)
            .filter(|key| *key != 0)
    }
}
pub(crate) fn modifiers(m: winit::keyboard::ModifiersState) -> u8 {
    u8::from(m.control_key())
        | (u8::from(m.shift_key()) << 1)
        | (u8::from(m.alt_key()) << 2)
        | (u8::from(m.super_key()) << 3)
}
fn path() -> Result<PathBuf> {
    let base = crate::platform::paths::local_app_data().context("无法确定本地设置目录")?;
    ensure!(base.is_absolute(), "设置目录必须为绝对路径");
    Ok(base.join("OpenUUYC").join("shortcuts.json"))
}
fn read(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 8192, "快捷键配置过大");
    Ok(Some(bytes))
}
fn refresh() {
    let revision = {
        let mut cache = runtime().cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache
            .checked
            .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
        {
            return;
        }
        cache.checked = Some(Instant::now());
        cache.revision
    };
    let result = path().and_then(|p| read(&p));
    apply_refresh(runtime(), revision, result);
}
fn apply_refresh(state: &Runtime, revision: u64, result: Result<Option<Vec<u8>>>) {
    let mut cache = state.cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.revision != revision {
        return;
    }
    cache.revision = cache.revision.wrapping_add(1);
    match result {
        Ok(bytes) => {
            if bytes == cache.bytes && cache.error.is_none() {
                return;
            }
            let parsed = bytes
                .as_ref()
                .map_or_else(
                    || Ok(Settings::default()),
                    |b| serde_json::from_slice::<Settings>(b).context("快捷键配置无效"),
                )
                .and_then(|s| {
                    s.validate()?;
                    Ok(s)
                });
            cache.bytes = bytes;
            match parsed {
                Ok(s) => {
                    state.active.store(s.packed(), Ordering::Release);
                    cache.error = None;
                }
                Err(e) => cache.error = Some(e.to_string()),
            }
        }
        Err(e) => cache.error = Some(e.to_string()),
    }
}
#[cfg(windows)]
struct FileGuard(HANDLE);
#[cfg(windows)]
impl Drop for FileGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}
fn save(expected: &Option<Vec<u8>>, settings: &Settings) -> Result<()> {
    settings.validate()?;
    #[cfg(windows)]
    let _guard = {
        let handle = unsafe {
            CreateMutexW(
                None,
                false,
                windows::core::w!("Local\\OpenUUYC.Shortcuts.Settings"),
            )
        }?;
        let wait = unsafe { WaitForSingleObject(handle, 3000) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            unsafe {
                let _ = CloseHandle(handle);
            }
            bail!("其他窗口正在保存快捷键，请稍后重试");
        }
        FileGuard(handle)
    };
    #[cfg(not(windows))]
    let _guard = settings_lock()?;
    let path = path()?;
    ensure!(
        &read(&path)? == expected,
        "快捷键已在其他窗口修改，请点击重新载入"
    );
    std::fs::create_dir_all(path.parent().context("设置目录无效")?)?;
    let bytes = serde_json::to_vec_pretty(settings)?;
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    let mut cache = runtime().cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.revision = cache.revision.wrapping_add(1);
    runtime().active.store(settings.packed(), Ordering::Release);
    cache.bytes = Some(bytes);
    cache.error = None;
    cache.checked = Some(Instant::now());
    Ok(())
}

#[derive(Default)]
pub(crate) struct Editor {
    draft: Option<Settings>,
    baseline: Option<Settings>,
    expected: Option<Vec<u8>>,
    recording: Option<Action>,
    pending_binding: Option<Binding>,
    pending_unsupported: bool,
    held_modifiers: u8,
    owner: u64,
    error: Option<String>,
}
impl Drop for Editor {
    fn drop(&mut self) {
        self.cancel_recording();
    }
}
impl Editor {
    pub fn reset(&mut self) {
        self.cancel_recording();
        self.draft = None;
        self.baseline = None;
        self.error = None;
    }
    pub fn cancel_recording(&mut self) {
        if self.owner != 0 {
            let _ = runtime().capture.compare_exchange(
                self.owner,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        self.recording = None;
        self.pending_binding = None;
        self.pending_unsupported = false;
        self.held_modifiers = 0;
        self.owner = 0;
    }
    fn finish_recording(&mut self, released: bool) {
        if !released {
            return;
        }
        let Some(action) = self.recording else {
            return;
        };
        if let Some(binding) = self.pending_binding {
            self.draft.as_mut().unwrap().bindings[action.index()] = Some(binding);
            self.error = None;
            self.cancel_recording();
        } else if self.pending_unsupported {
            self.pending_unsupported = false;
            self.error = Some("此按键暂不支持，请尝试其他按键或组合键".into());
        }
    }
    fn reload(&mut self) {
        self.cancel_recording();
        let cache = runtime().cache.lock().unwrap_or_else(|e| e.into_inner());
        let s = Settings::unpack(revision());
        self.draft = Some(s.clone());
        self.baseline = Some(s);
        self.expected = cache.bytes.clone();
        self.error = cache.error.clone();
    }
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        if self.recording.is_none()
            && self.draft.is_some()
            && self.draft == self.baseline
            && self.expected
                != runtime()
                    .cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .bytes
        {
            self.reload();
        }
        if self.draft.is_none() {
            self.reload();
        }
        if !ui.input(|i| i.viewport().focused.unwrap_or(false)) {
            self.cancel_recording();
        }
        let was_recording = self.recording;
        if was_recording.is_some() {
            for event in ui.input(|i| i.events.clone()) {
                if let egui::Event::Key { key, pressed, .. } = &event {
                    let modifier = match key {
                        egui::Key::ControlLeft => Some((0, 1)),
                        egui::Key::ControlRight => Some((1, 1)),
                        egui::Key::ShiftLeft => Some((2, 2)),
                        egui::Key::ShiftRight => Some((3, 2)),
                        egui::Key::AltLeft => Some((4, 4)),
                        egui::Key::AltRight => Some((5, 4)),
                        egui::Key::SuperLeft => Some((6, 8)),
                        egui::Key::SuperRight => Some((7, 8)),
                        _ => None,
                    };
                    if let Some((bit, mask)) = modifier {
                        if *pressed {
                            self.held_modifiers |= 1 << bit;
                            if let Some(binding) = &mut self.pending_binding {
                                binding.modifiers |= mask;
                            }
                        } else {
                            self.held_modifiers &= !(1 << bit);
                        }
                        self.error = None;
                        continue;
                    }
                }
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = event
                {
                    if key == egui::Key::Escape
                        && !modifiers.ctrl
                        && !modifiers.alt
                        && !modifiers.shift
                    {
                        self.cancel_recording();
                        break;
                    }
                    self.error = None;
                    let symbol = key.symbol_or_name();
                    #[cfg(windows)]
                    let vk = if symbol.chars().count() == 1 {
                        let value = unsafe {
                            windows::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW(
                                symbol.chars().next().unwrap() as u16,
                            )
                        };
                        (value != -1).then_some((value as u16) & 255)
                    } else {
                        None
                    };
                    // Without a layout query, only the ASCII names egui reports
                    // map back to a virtual key.
                    #[cfg(not(windows))]
                    let vk = symbol
                        .chars()
                        .next()
                        .filter(|_| symbol.chars().count() == 1)
                        .and_then(ascii_virtual_key);
                    #[cfg_attr(
                        not(windows),
                        allow(unused_mut, reason = "Keypad disambiguation is Windows-only.")
                    )]
                    if let Some(mut key) = vk.or_else(|| crate::plugins::hotkeys::key_code(key)) {
                        // egui merges keypad digits into the corresponding textual key.
                        #[cfg(windows)]
                        if (48..=57).contains(&key)
                            && unsafe {
                                windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(
                                    i32::from(key + 48),
                                )
                            } < 0
                        {
                            key += 48;
                        }
                        #[cfg(windows)]
                        for (regular, keypad) in [(190, 110), (187, 107), (189, 109), (191, 111)] {
                            if key == regular
                                && unsafe {
                                    windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(
                                        keypad,
                                    )
                                } < 0
                            {
                                key = keypad as u16;
                            }
                        }
                        #[cfg(windows)]
                        let win = self.held_modifiers & 0xc0 != 0
                            || unsafe {
                                windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(91)
                                    < 0
                                    || windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(
                                        92,
                                    ) < 0
                            };
                        #[cfg(not(windows))]
                        let win = self.held_modifiers & 0xc0 != 0;
                        let b = Binding {
                            key,
                            modifiers: u8::from(modifiers.ctrl)
                                | (u8::from(modifiers.shift) << 1)
                                | (u8::from(modifiers.alt) << 2)
                                | (u8::from(win) << 3),
                        };
                        self.pending_binding = Some(b);
                        self.pending_unsupported = false;
                    } else {
                        self.pending_unsupported = self.pending_binding.is_none();
                    }
                }
            }
        }
        if self.recording.is_some() {
            #[cfg(windows)]
            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
            #[cfg(windows)]
            let win = unsafe { GetAsyncKeyState(91) < 0 || GetAsyncKeyState(92) < 0 };
            // egui does not report the Super modifier on Linux.
            #[cfg(not(windows))]
            let win = false;
            let (keys_down, mods) = ui.input(|i| (!i.keys_down.is_empty(), i.modifiers));
            if keys_down && let Some(binding) = &mut self.pending_binding {
                binding.modifiers |= u8::from(mods.ctrl)
                    | (u8::from(mods.shift) << 1)
                    | (u8::from(mods.alt) << 2)
                    | (u8::from(win) << 3);
            }
            let released = !keys_down
                && !mods.any()
                && !win
                && [16, 17, 18].into_iter().all(|vk| modifier_released(vk));
            if !released {
                self.error = None;
            }
            self.finish_recording(released);
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
        ui.label(
            egui::RichText::new("在播放窗口内生效 · 所有设备通用")
                .size(crate::ui::theme::SMALL)
                .color(crate::ui::theme::MUTED),
        );
        ui.add_space(18.0);
        egui::Frame::new()
            .stroke(egui::Stroke::new(1.0, crate::ui::theme::LINE))
            .corner_radius(crate::ui::theme::PANEL_RADIUS)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.set_width(ui.available_width());
                for (index, action) in Action::ALL.into_iter().enumerate() {
                    let binding =
                        self.draft.as_ref().unwrap().bindings[action.index()].map(Binding::label);
                    match crate::ui::controls::shortcut_row(
                        ui,
                        index,
                        action.label(),
                        binding.as_deref(),
                        self.recording == Some(action),
                        self.pending_binding
                            .filter(|_| self.recording == Some(action))
                            .map(Binding::label)
                            .as_deref(),
                        index == 3,
                    ) {
                        crate::ui::controls::ShortcutRowAction::None => {}
                        crate::ui::controls::ShortcutRowAction::Edit => {
                            self.cancel_recording();
                            self.recording = Some(action);
                            self.owner = foreground_window();
                            runtime().capture.store(self.owner, Ordering::Release);
                            self.error = None;
                        }
                        crate::ui::controls::ShortcutRowAction::Cancel => self.cancel_recording(),
                        crate::ui::controls::ShortcutRowAction::Clear => {
                            self.draft.as_mut().unwrap().bindings[action.index()] = None;
                            self.cancel_recording();
                            self.error = None;
                        }
                    }
                }
            });
        ui.add_space(16.0);
        let validation = if self.recording.is_none() {
            self.draft
                .as_ref()
                .unwrap()
                .validate()
                .err()
                .map(|e| e.to_string())
        } else {
            None
        };
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "shortcut-settings-error",
            "快捷键设置",
            crate::ui::controls::DialogIcon::Error,
            validation.as_deref().or(self.error.as_deref()),
        );
        let dirty = self.draft != self.baseline;
        ui.horizontal(|ui| {
            if ui
                .add(crate::ui::controls::quiet_button("恢复默认"))
                .on_hover_text("恢复默认组合，保存后生效")
                .clicked()
            {
                self.cancel_recording();
                self.draft = Some(Settings::default());
                self.error = None;
            }
            if dirty {
                ui.label(
                    egui::RichText::new("未保存")
                        .size(crate::ui::theme::SMALL)
                        .color(crate::ui::theme::MUTED),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        validation.is_none()
                            && self.recording.is_none()
                            && (dirty || self.error.is_some()),
                        crate::ui::controls::primary("保存更改"),
                    )
                    .clicked()
                {
                    match save(&self.expected, self.draft.as_ref().unwrap()) {
                        Ok(()) => self.reload(),
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                if (dirty || self.recording.is_some())
                    && ui.add(crate::ui::controls::secondary("取消修改")).clicked()
                {
                    self.reload();
                }
                if self.error.is_some()
                    && ui.add(crate::ui::controls::secondary("重新载入")).clicked()
                {
                    self.reload();
                }
            });
        });
    }
}

/// The settings file is shared by every window of every running client, so the
/// rewrite is serialized with an advisory lock beside the file itself.
#[cfg(not(windows))]
fn settings_lock() -> Result<std::fs::File> {
    let path = path()?.with_extension("lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .context("打开快捷键配置锁")?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(anyhow::Error::new(error).context("锁定快捷键配置失败"));
            }
        }
        if Instant::now() >= deadline {
            bail!("其他窗口正在保存快捷键，请稍后重试");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// egui reports held modifiers itself; a key that is not held cannot be probed.
#[cfg(not(windows))]
const fn modifier_released(_key: i32) -> bool {
    true
}

#[cfg(windows)]
fn modifier_released(key: i32) -> bool {
    unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(key) >= 0 }
}

/// US-layout virtual keys for the printable characters egui names.
#[cfg(not(windows))]
fn ascii_virtual_key(symbol: char) -> Option<u16> {
    let upper = symbol.to_ascii_uppercase();
    Some(match upper {
        'A'..='Z' | '0'..='9' => upper as u16,
        ' ' => 32,
        ';' | ':' => 186,
        '=' | '+' => 187,
        ',' | '<' => 188,
        '-' | '_' => 189,
        '.' | '>' => 190,
        '/' | '?' => 191,
        '`' | '~' => 192,
        '[' | '{' => 219,
        '\\' | '|' => 220,
        ']' | '}' => 221,
        '\'' | '"' => 222,
        _ => return None,
    })
}
