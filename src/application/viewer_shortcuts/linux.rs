use super::{Action, Binding, Editor, Watcher, match_key};
use anyhow::{Context, Result};
use egui::{Context as EguiContext, Event, Key, Modifiers};
use std::{fs::File, path::Path};

// Use the same persisted codes as the shared shortcut model. Egui does not
// distinguish every native key (notably keypad digits) on Linux.
const SPECIAL_KEYS: &[(Key, u16)] = &[
    (Key::Backspace, 8),
    (Key::Tab, 9),
    (Key::Enter, 13),
    (Key::Escape, 27),
    (Key::Space, 32),
    (Key::PageUp, 33),
    (Key::PageDown, 34),
    (Key::End, 35),
    (Key::Home, 36),
    (Key::ArrowLeft, 37),
    (Key::ArrowUp, 38),
    (Key::ArrowRight, 39),
    (Key::ArrowDown, 40),
    (Key::Insert, 45),
    (Key::Delete, 46),
    (Key::Semicolon, 186),
    (Key::Equals, 187),
    (Key::Comma, 188),
    (Key::Minus, 189),
    (Key::Period, 190),
    (Key::Slash, 191),
    (Key::Backtick, 192),
    (Key::OpenBracket, 219),
    (Key::Backslash, 220),
    (Key::CloseBracket, 221),
    (Key::Quote, 222),
];

fn key_code(key: Key) -> Option<u16> {
    let name = key.name();
    if let [c] = name.as_bytes()
        && c.is_ascii_alphanumeric()
    {
        return Some(u16::from(c.to_ascii_uppercase()));
    }
    if let Some(f) = name.strip_prefix('F').and_then(|n| n.parse::<u16>().ok())
        && (1..=24).contains(&f)
    {
        return Some(111 + f);
    }
    SPECIAL_KEYS.iter().find_map(|&(k, code)| (k == key).then_some(code))
}

pub(super) fn supports(code: u16) -> bool {
    matches!(code, 48..=57 | 65..=90 | 112..=135)
        || SPECIAL_KEYS.iter().any(|&(_, special)| special == code)
}

pub(super) fn label(binding: Binding) -> String {
    let mut names = Vec::new();
    for (bit, name) in [(1, "Ctrl"), (2, "Shift"), (4, "Alt"), (8, "Super")] {
        if binding.modifiers & bit != 0 {
            names.push(name.to_owned());
        }
    }
    names.push(match binding.key {
        48..=57 | 65..=90 => char::from(binding.key as u8).to_string(),
        112..=135 => format!("F{}", binding.key - 111),
        code => SPECIAL_KEYS.iter().find(|&&(_, n)| n == code).map_or_else(
            || format!("Key {code}"),
            |(key, _)| key.symbol_or_name().to_owned(),
        ),
    });
    names.join(" + ")
}

pub(super) fn lock_settings(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.try_lock().context(crate::i18n::t().viewer.shortcut_saving_elsewhere)?;
    Ok(lock)
}

// Egui's `command` aliases Ctrl on Linux, not Super. Track its two physical
// keys separately so pressing both and releasing one does not clear Super.
fn track_super(held: &mut u8, key: Key, pressed: bool) {
    let bit = match key {
        Key::SuperLeft => 1,
        Key::SuperRight => 2,
        _ => return,
    };
    if pressed {
        *held |= bit;
    } else {
        *held &= !bit;
    }
}

fn modifiers(mods: Modifiers, super_held: u8) -> u8 {
    u8::from(mods.ctrl)
        | (u8::from(mods.shift) << 1)
        | (u8::from(mods.alt) << 2)
        | (u8::from(super_held != 0 || mods.mac_cmd) << 3)
}

pub(crate) struct WindowShortcuts {
    _watcher: Option<Watcher>,
    consumed: Vec<Key>,
    super_held: u8,
}

impl WindowShortcuts {
    pub(crate) fn new() -> Self {
        Self {
            _watcher: Watcher::new()
                .inspect_err(|error| tracing::warn!(%error, "shortcut settings watcher unavailable"))
                .ok(),
            consumed: Vec::new(),
            super_held: 0,
        }
    }

    pub(crate) fn take(&mut self, ctx: &EguiContext) -> Vec<Action> {
        let mut actions = Vec::new();
        let text_editing = ctx.text_edit_focused();
        ctx.input_mut(|input| {
            if !input.focused {
                self.super_held = 0;
                self.consumed.clear();
                return;
            }
            input.events.retain(|event| {
                let Event::Key { key, physical_key, pressed, repeat, modifiers: mods } = *event else {
                    return true;
                };
                let key = physical_key.unwrap_or(key);
                track_super(&mut self.super_held, key, pressed);
                if let Some(index) = self.consumed.iter().position(|&held| held == key) {
                    if !pressed {
                        self.consumed.swap_remove(index);
                    }
                    return false;
                }
                if !text_editing && pressed && !repeat
                    && let Some(action) = key_code(key)
                        .and_then(|code| match_key(code, modifiers(mods, self.super_held)))
                {
                    self.consumed.push(key);
                    actions.push(action);
                    return false;
                }
                true
            });
        });
        actions
    }
}

impl Editor {
    pub(super) fn record_linux(&mut self, ui: &egui::Ui) {
        let (events, released) = ui.input_mut(|input| {
            let mut events = Vec::new();
            input.events.retain(|event| match event {
                Event::Key { .. } => {
                    events.push(event.clone());
                    false
                }
                Event::Text(_) | Event::Copy | Event::Cut | Event::Paste(_) => false,
                _ => true,
            });
            (events, input.keys_down.is_empty() && input.modifiers.is_none())
        });
        for event in events {
            let Event::Key { key, physical_key, pressed, repeat, modifiers: mods } = event else {
                continue;
            };
            let key = physical_key.unwrap_or(key);
            track_super(&mut self.held_modifiers, key, pressed);
            if !pressed || repeat || matches!(key,
                Key::SuperLeft | Key::SuperRight | Key::ControlLeft | Key::ControlRight
                | Key::ShiftLeft | Key::ShiftRight | Key::AltLeft | Key::AltRight)
            {
                continue;
            }
            let mods = modifiers(mods, self.held_modifiers);
            if key == Key::Escape && mods == 0 {
                self.cancel_recording();
                return;
            }
            self.error = None;
            self.pending_binding = key_code(key).map(|key| Binding { key, modifiers: mods });
            self.pending_unsupported = self.pending_binding.is_none();
        }
        self.finish_recording(released && self.held_modifiers == 0);
    }
}
