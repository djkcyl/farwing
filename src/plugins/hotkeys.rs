//! Foreground-only graph activation signals. No input injection or aiming policy.
use super::*;
use std::sync::{Arc, Mutex, OnceLock};
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub key: u16,
    pub modifiers: u8,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Trigger,
    #[default]
    Toggle,
    Hold,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcut {
    pub binding: Option<Binding>,
    pub mode: Mode,
    /// An explicitly disabled condition remains blocking until edited.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Spec {
    pub id: u64,
    #[serde(default)]
    pub binding: Option<Binding>,
    #[serde(default)]
    pub mode: Mode,
}
#[derive(Default)]
struct State {
    active: bool,
    epoch: u64,
    pulse_until: Option<std::time::Instant>,
}
pub struct Gate {
    pub spec: Spec,
    state: Mutex<State>,
}
impl Gate {
    pub fn new(spec: Spec) -> Arc<Self> {
        Arc::new(Self {
            spec,
            state: Mutex::new(State::default()),
        })
    }
    pub fn snapshot(&self) -> (bool, u64) {
        let s = super::process::lock(&self.state);
        (
            s.active
                && s.pulse_until
                    .is_none_or(|until| std::time::Instant::now() <= until),
            s.epoch,
        )
    }
    pub fn reset(&self) {
        let mut s = super::process::lock(&self.state);
        if s.active {
            s.active = false;
            s.epoch = s.epoch.wrapping_add(1);
        }
    }
    fn press(&self) {
        let mut s = super::process::lock(&self.state);
        s.epoch = s.epoch.wrapping_add(1);
        s.pulse_until = (self.spec.mode == Mode::Trigger)
            .then(|| std::time::Instant::now() + std::time::Duration::from_millis(750));
        s.active = if self.spec.mode == Mode::Toggle {
            !s.active
        } else {
            true
        };
    }
}
pub struct Condition {
    pub port: String,
    pub name: String,
    pub gate: Option<Arc<Gate>>,
}
#[derive(Default)]
struct CombinedState {
    inputs: Vec<(bool, u64)>,
    epoch: u64,
    blocked: bool,
    pulse_epoch: Option<u64>,
    pulse_valid: bool,
}
/// Per-consumer conjunction. Raw hotkeys remain independent and may be shared.
pub struct ControlGate {
    pub conditions: Vec<Condition>,
    state: Mutex<CombinedState>,
}
impl ControlGate {
    pub fn new(conditions: Vec<Condition>) -> Arc<Self> {
        Arc::new(Self {
            conditions,
            state: Mutex::new(CombinedState::default()),
        })
    }
    pub fn is_trigger(&self) -> bool {
        self.conditions.iter().any(|c| {
            c.gate
                .as_ref()
                .is_some_and(|g| g.spec.mode == Mode::Trigger)
        })
    }
    pub fn snapshot(&self) -> (bool, u64) {
        let mut state = super::process::lock(&self.state);
        let inputs: Vec<_> = self
            .conditions
            .iter()
            .map(|c| c.gate.as_ref().map_or((false, 0), |g| g.snapshot()))
            .collect();
        let all = !inputs.is_empty() && inputs.iter().all(|(active, _)| *active);
        let pulse = self.conditions.iter().position(|c| {
            c.gate
                .as_ref()
                .is_some_and(|g| g.spec.mode == Mode::Trigger)
        });
        if state.inputs != inputs {
            if let Some(index) = pulse {
                if state.pulse_epoch != Some(inputs[index].1) {
                    state.pulse_epoch = Some(inputs[index].1);
                    state.pulse_valid = all;
                } else if !all {
                    state.pulse_valid = false;
                }
            }
            state.inputs = inputs;
            state.blocked = false;
            state.epoch = state.epoch.wrapping_add(1);
        }
        (
            all && !state.blocked && (pulse.is_none() || state.pulse_valid),
            state.epoch,
        )
    }
    pub fn reset(&self) {
        self.reset_epoch(self.snapshot().1);
    }
    pub fn reset_epoch(&self, epoch: u64) {
        let _ = self.snapshot();
        let mut state = super::process::lock(&self.state);
        if state.epoch == epoch && !state.blocked {
            state.blocked = true;
            state.pulse_valid = false;
            state.epoch = state.epoch.wrapping_add(1);
        }
    }
    pub fn suspended(&self) -> bool {
        let _ = self.snapshot();
        super::process::lock(&self.state).blocked
    }
    pub fn master_enabled(&self) -> bool {
        !self.conditions.is_empty()
            && self
                .conditions
                .iter()
                .find(|c| c.port == "enabled")
                .is_none_or(|c| c.gate.as_ref().is_some_and(|g| g.snapshot().0))
    }
    pub fn binding_text(&self) -> String {
        if self.conditions.is_empty() {
            return "未绑定快捷键".into();
        }
        self.conditions
            .iter()
            .map(|c| {
                c.gate
                    .as_ref()
                    .and_then(|g| {
                        g.spec.binding.as_ref().map(|b| {
                            let key = label(b);
                            match g.spec.mode {
                                Mode::Hold => format!("按住 {key}"),
                                Mode::Trigger => format!("{key} 单次"),
                                Mode::Toggle => key,
                            }
                        })
                    })
                    .unwrap_or_else(|| format!("{}未绑定", c.name))
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

struct Registration {
    gates: Vec<Arc<Gate>>,
    controls: Vec<Arc<ControlGate>>,
    allowed: bool,
}
#[derive(Default)]
struct Registry {
    windows: std::collections::BTreeMap<u64, Registration>,
    held: std::collections::BTreeSet<u16>,
    consumed: std::collections::BTreeSet<u16>,
}
fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}
pub fn register(owner: u64, gates: Vec<Arc<Gate>>, controls: Vec<Arc<ControlGate>>, allowed: bool) {
    let mut r = super::process::lock(registry());
    if allowed && r.windows.get(&owner).is_none_or(|old| !old.allowed) {
        let keys = gates
            .iter()
            .filter_map(|g| g.spec.binding.as_ref().map(|b| b.key))
            .chain([91, 92, 160, 161, 162, 163, 164, 165])
            .collect::<Vec<_>>();
        for key in keys {
            if unsafe {
                windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(i32::from(key))
            } < 0
            {
                r.held.insert(key);
            } else {
                r.held.remove(&key);
            }
        }
    }
    if !allowed {
        for gate in &gates {
            gate.reset();
        }
    }
    r.windows.insert(
        owner,
        Registration {
            gates,
            controls,
            allowed,
        },
    );
}
pub fn unregister(owner: u64) {
    if let Some(r) = super::process::lock(registry()).windows.remove(&owner) {
        for g in r.gates {
            g.reset();
        }
    }
}
pub fn key_event(owner: u64, key: u16, down: bool) -> bool {
    let mut r = super::process::lock(registry());
    let was = r.held.contains(&key);
    if down {
        r.held.insert(key);
    } else {
        r.held.remove(&key);
    }
    let modifiers = (u8::from(r.held.contains(&162) || r.held.contains(&163)))
        | (u8::from(r.held.contains(&160) || r.held.contains(&161)) << 1)
        | (u8::from(r.held.contains(&164) || r.held.contains(&165)) << 2)
        | (u8::from(r.held.contains(&91) || r.held.contains(&92)) << 3);
    let mut consume = if down {
        r.consumed.contains(&key)
    } else {
        r.consumed.remove(&key)
    };
    for (registered_owner, reg) in &r.windows {
        for gate in &reg.gates {
            let Some(binding) = &gate.spec.binding else {
                continue;
            };
            let matches = r.held.contains(&binding.key) && modifiers == binding.modifiers;
            if gate.spec.mode == Mode::Hold && !matches {
                gate.reset();
            }
            if *registered_owner == owner && down && !was && binding.key == key && matches {
                consume = true;
                if reg.allowed {
                    gate.press();
                }
            }
        }
    }
    // Observe every physical edge, including master-off while a pulse is live.
    // Otherwise a rapid off/on between inference frames could replay that pulse.
    for registration in r.windows.values() {
        for control in &registration.controls {
            let _ = control.snapshot();
        }
    }
    if down && consume {
        r.consumed.insert(key);
    }
    consume
}
pub fn key_code(key: egui::Key) -> Option<u16> {
    let name = key.name();
    if name.len() == 1 {
        let b = name.as_bytes()[0].to_ascii_uppercase();
        if b.is_ascii_alphanumeric() {
            return Some(u16::from(b));
        }
    }
    if let Some(f) = name.strip_prefix('F').and_then(|n| n.parse::<u16>().ok())
        && (1..=24).contains(&f)
    {
        return Some(111 + f);
    }
    Some(match key {
        egui::Key::ArrowLeft => 37,
        egui::Key::ArrowUp => 38,
        egui::Key::ArrowRight => 39,
        egui::Key::ArrowDown => 40,
        egui::Key::Escape => 27,
        egui::Key::Tab => 9,
        egui::Key::Backspace => 8,
        egui::Key::Enter => 13,
        egui::Key::Space => 32,
        egui::Key::Insert => 45,
        egui::Key::Delete => 46,
        egui::Key::Home => 36,
        egui::Key::End => 35,
        egui::Key::PageUp => 33,
        egui::Key::PageDown => 34,
        _ => return None,
    })
}
pub fn valid(binding: &Binding) -> bool {
    (matches!(binding.key, 1 | 2 | 4 | 5 | 6) || (8..=254).contains(&binding.key))
        && !matches!(binding.key,16..=18|91..=92|160..=165)
        && binding.modifiers <= 15
        && crate::application::viewer_shortcuts::match_key(binding.key, binding.modifiers).is_none()
}
pub fn label(binding: &Binding) -> String {
    let mut words = Vec::new();
    for (bit, name) in [(1, "Ctrl"), (2, "Shift"), (4, "Alt"), (8, "Win")] {
        if binding.modifiers & bit != 0 {
            words.push(name.to_owned());
        }
    }
    let key = match binding.key {
        1 => "鼠标左键".into(),
        2 => "鼠标右键".into(),
        4 => "鼠标中键".into(),
        5 => "鼠标侧键1".into(),
        6 => "鼠标侧键2".into(),
        65..=90 | 48..=57 => char::from_u32(u32::from(binding.key))
            .unwrap_or('?')
            .to_string(),
        112..=135 => format!("F{}", binding.key - 111),
        8 => "Backspace".into(),
        9 => "Tab".into(),
        13 => "Enter".into(),
        27 => "Esc".into(),
        32 => "Space".into(),
        37 => "←".into(),
        38 => "↑".into(),
        39 => "→".into(),
        40 => "↓".into(),
        _ => format!("Key {}", binding.key),
    };
    words.push(key);
    words.join(" + ")
}
