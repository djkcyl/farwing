//! Physical key identity for INPUT v2 (`docs/protocol/v2-design.md` §7).
//!
//! Keys travel as HID usages `(page << 16) | id` (keyboard page 0x07,
//! consumer page 0x0C). The controller derives the usage from the set-1 scan
//! code it observes (E0-extended codes written as `0xE0xx`, following the
//! Windows convention that reports NumLock as `0xE045` and Pause as `0x45`);
//! the host injects the inverse scan code, so its own layout and IME decide
//! what the key means.

pub(crate) const KEYBOARD: u32 = 0x07 << 16;
pub(crate) const CONSUMER: u32 = 0x0C << 16;

const K: u32 = KEYBOARD;
const C: u32 = CONSUMER;

/// `(scan, usage)`. Scan → usage takes the first entry for a scan code and
/// usage → scan the first entry for a usage; aliases come after the primary.
const KEYS: &[(u16, u32)] = &[
    (0x01, K | 0x29),    // Escape
    (0x02, K | 0x1e),    // 1
    (0x03, K | 0x1f),    // 2
    (0x04, K | 0x20),    // 3
    (0x05, K | 0x21),    // 4
    (0x06, K | 0x22),    // 5
    (0x07, K | 0x23),    // 6
    (0x08, K | 0x24),    // 7
    (0x09, K | 0x25),    // 8
    (0x0a, K | 0x26),    // 9
    (0x0b, K | 0x27),    // 0
    (0x0c, K | 0x2d),    // -
    (0x0d, K | 0x2e),    // =
    (0x0e, K | 0x2a),    // Backspace
    (0x0f, K | 0x2b),    // Tab
    (0x10, K | 0x14),    // Q
    (0x11, K | 0x1a),    // W
    (0x12, K | 0x08),    // E
    (0x13, K | 0x15),    // R
    (0x14, K | 0x17),    // T
    (0x15, K | 0x1c),    // Y
    (0x16, K | 0x18),    // U
    (0x17, K | 0x0c),    // I
    (0x18, K | 0x12),    // O
    (0x19, K | 0x13),    // P
    (0x1a, K | 0x2f),    // [
    (0x1b, K | 0x30),    // ]
    (0x1c, K | 0x28),    // Enter
    (0x1d, K | 0xe0),    // Left Ctrl
    (0x1e, K | 0x04),    // A
    (0x1f, K | 0x16),    // S
    (0x20, K | 0x07),    // D
    (0x21, K | 0x09),    // F
    (0x22, K | 0x0a),    // G
    (0x23, K | 0x0b),    // H
    (0x24, K | 0x0d),    // J
    (0x25, K | 0x0e),    // K
    (0x26, K | 0x0f),    // L
    (0x27, K | 0x33),    // ;
    (0x28, K | 0x34),    // '
    (0x29, K | 0x35),    // ` (Zenkaku/Hankaku on JIS)
    (0x2a, K | 0xe1),    // Left Shift
    (0x2b, K | 0x31),    // \ (ISO #)
    (0x2c, K | 0x1d),    // Z
    (0x2d, K | 0x1b),    // X
    (0x2e, K | 0x06),    // C
    (0x2f, K | 0x19),    // V
    (0x30, K | 0x05),    // B
    (0x31, K | 0x11),    // N
    (0x32, K | 0x10),    // M
    (0x33, K | 0x36),    // ,
    (0x34, K | 0x37),    // .
    (0x35, K | 0x38),    // /
    (0x36, K | 0xe5),    // Right Shift
    (0x37, K | 0x55),    // Keypad *
    (0x38, K | 0xe2),    // Left Alt
    (0x39, K | 0x2c),    // Space
    (0x3a, K | 0x39),    // Caps Lock
    (0x3b, K | 0x3a),    // F1
    (0x3c, K | 0x3b),    // F2
    (0x3d, K | 0x3c),    // F3
    (0x3e, K | 0x3d),    // F4
    (0x3f, K | 0x3e),    // F5
    (0x40, K | 0x3f),    // F6
    (0x41, K | 0x40),    // F7
    (0x42, K | 0x41),    // F8
    (0x43, K | 0x42),    // F9
    (0x44, K | 0x43),    // F10
    (0x45, K | 0x48),    // Pause (E1 1D 45 on the wire, reported as 45)
    (0x46, K | 0x47),    // Scroll Lock
    (0x47, K | 0x5f),    // Keypad 7
    (0x48, K | 0x60),    // Keypad 8
    (0x49, K | 0x61),    // Keypad 9
    (0x4a, K | 0x56),    // Keypad -
    (0x4b, K | 0x5c),    // Keypad 4
    (0x4c, K | 0x5d),    // Keypad 5
    (0x4d, K | 0x5e),    // Keypad 6
    (0x4e, K | 0x57),    // Keypad +
    (0x4f, K | 0x59),    // Keypad 1
    (0x50, K | 0x5a),    // Keypad 2
    (0x51, K | 0x5b),    // Keypad 3
    (0x52, K | 0x62),    // Keypad 0
    (0x53, K | 0x63),    // Keypad .
    (0x56, K | 0x64),    // ISO \ (102nd key)
    (0x57, K | 0x44),    // F11
    (0x58, K | 0x45),    // F12
    (0x59, K | 0x67),    // Keypad =
    (0x64, K | 0x68),    // F13
    (0x65, K | 0x69),    // F14
    (0x66, K | 0x6a),    // F15
    (0x67, K | 0x6b),    // F16
    (0x68, K | 0x6c),    // F17
    (0x69, K | 0x6d),    // F18
    (0x6a, K | 0x6e),    // F19
    (0x6b, K | 0x6f),    // F20
    (0x6c, K | 0x70),    // F21
    (0x6d, K | 0x71),    // F22
    (0x6e, K | 0x72),    // F23
    (0x70, K | 0x88),    // Katakana/Hiragana (International 2)
    (0x73, K | 0x87),    // Ro (International 1)
    (0x76, K | 0x73),    // F24
    (0x79, K | 0x8a),    // Henkan (International 4)
    (0x7b, K | 0x8b),    // Muhenkan (International 5)
    (0x7d, K | 0x89),    // Yen (International 3)
    (0x7e, K | 0x85),    // Keypad , (Brazil)
    (0xf1, K | 0x91),    // Hanja (LANG2)
    (0xf2, K | 0x90),    // Hangul (LANG1)
    (0xe01c, K | 0x58),  // Keypad Enter
    (0xe01d, K | 0xe4),  // Right Ctrl
    (0xe035, K | 0x54),  // Keypad /
    (0xe037, K | 0x46),  // Print Screen
    (0xe038, K | 0xe6),  // Right Alt
    (0xe045, K | 0x53),  // Num Lock
    (0xe047, K | 0x4a),  // Home
    (0xe048, K | 0x52),  // Up
    (0xe049, K | 0x4b),  // Page Up
    (0xe04b, K | 0x50),  // Left
    (0xe04d, K | 0x4f),  // Right
    (0xe04f, K | 0x4d),  // End
    (0xe050, K | 0x51),  // Down
    (0xe051, K | 0x4e),  // Page Down
    (0xe052, K | 0x49),  // Insert
    (0xe053, K | 0x4c),  // Delete
    (0xe05b, K | 0xe3),  // Left Windows
    (0xe05c, K | 0xe7),  // Right Windows
    (0xe05d, K | 0x65),  // Application
    (0xe010, C | 0xb6),  // Previous Track
    (0xe019, C | 0xb5),  // Next Track
    (0xe020, C | 0xe2),  // Mute
    (0xe021, C | 0x192), // Calculator
    (0xe022, C | 0xcd),  // Play/Pause
    (0xe024, C | 0xb7),  // Stop
    (0xe02e, C | 0xea),  // Volume Down
    (0xe030, C | 0xe9),  // Volume Up
    (0xe032, C | 0x223), // Browser Home
    (0xe065, C | 0x221), // Browser Search
    (0xe066, C | 0x22a), // Browser Favorites
    (0xe067, C | 0x227), // Browser Refresh
    (0xe068, C | 0x226), // Browser Stop
    (0xe069, C | 0x225), // Browser Forward
    (0xe06a, C | 0x224), // Browser Back
    (0xe06b, C | 0x194), // My Computer
    (0xe06c, C | 0x18a), // Mail
    (0xe06d, C | 0x183), // Media Select
    // Aliases.
    (0x54, K | 0x46),   // Alt+Print Screen (SysRq)
    (0xe046, K | 0x48), // Ctrl+Pause (Break)
    (0x71, K | 0x91),   // Hanja on some Korean keyboards
    (0x72, K | 0x90),   // Hangul on some Korean keyboards
    (0x2b, K | 0x32),   // ISO # shares the US \ position
];

/// Usage of a set-1 scan code (`0xE0xx` for E0-extended keys).
pub(crate) fn usage(scan: u16) -> Option<u32> {
    KEYS.iter().find(|(s, _)| *s == scan).map(|(_, u)| *u)
}

/// Scan code the host injects for a usage.
pub(crate) fn scan(usage: u32) -> Option<u16> {
    KEYS.iter().find(|(_, u)| *u == usage).map(|(s, _)| *s)
}

/// Caps Lock, Num Lock and Scroll Lock, in `LockState` field order.
pub(crate) const LOCKS: [u32; 3] = [K | 0x39, K | 0x53, K | 0x47];

pub(crate) fn lock_index(usage: u32) -> Option<usize> {
    LOCKS.iter().position(|lock| *lock == usage)
}

/// Shift, Ctrl, Alt and Windows keys on either side.
pub(crate) fn modifier(usage: u32) -> bool {
    (K | 0xe0..=K | 0xe7).contains(&usage)
}

/// Keys whose virtual key identifies them regardless of layout or of how the
/// scan code was reported (shared 0x45, media keys sent without a scan code).
#[cfg(windows)]
fn fixed(vk: u16) -> Option<u32> {
    Some(match vk {
        0x13 => K | 0x48, // Pause
        0x90 => K | 0x53, // Num Lock
        0xa6 => C | 0x224,
        0xa7 => C | 0x225,
        0xa8 => C | 0x227,
        0xa9 => C | 0x226,
        0xaa => C | 0x221,
        0xab => C | 0x22a,
        0xac => C | 0x223,
        0xad => C | 0xe2,
        0xae => C | 0xea,
        0xaf => C | 0xe9,
        0xb0 => C | 0xb5,
        0xb1 => C | 0xb6,
        0xb2 => C | 0xb7,
        0xb3 => C | 0xcd,
        0xb4 => C | 0x18a,
        0xb5 => C | 0x183,
        0xb6 => C | 0x194,
        0xb7 => C | 0x192,
        _ => return None,
    })
}

/// Controller side: the usage of an observed key event. `scan` is the
/// reported scan code with `0xE000` for extended keys; zero (input injected
/// without one) falls back to the local layout's code for `vk`.
#[cfg(windows)]
pub(crate) fn observed(vk: u16, scan: u16) -> Option<u32> {
    if let Some(usage) = fixed(vk) {
        return Some(usage);
    }
    if scan & 0xff != 0 {
        return usage(scan);
    }
    usage(virtual_scan(vk))
}

/// Scan code of a virtual key in the foreground layout, `0xE0xx` when extended.
#[cfg(windows)]
pub(crate) fn virtual_scan(key: u16) -> u16 {
    // These logical keys belong to the dedicated navigation cluster. Some
    // Windows layouts return the shared keypad scan without E0 even for
    // MAPVK_VK_TO_VSC_EX, so their identity must come from the VK.
    match key {
        0x21 => 0xe049, // Page Up
        0x22 => 0xe051, // Page Down
        0x23 => 0xe04f, // End
        0x24 => 0xe047, // Home
        0x25 => 0xe04b, // Left
        0x26 => 0xe048, // Up
        0x27 => 0xe04d, // Right
        0x28 => 0xe050, // Down
        0x2d => 0xe052, // Insert
        0x2e => 0xe053, // Delete
        _ => {
            use windows::Win32::UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*};
            let scan = unsafe {
                MapVirtualKeyExW(
                    u32::from(key),
                    MAPVK_VK_TO_VSC_EX,
                    Some(GetKeyboardLayout(GetWindowThreadProcessId(
                        GetForegroundWindow(),
                        None,
                    ))),
                )
            };
            // E1 sequences (Pause) have no single-prefix form.
            if scan & 0xff00 == 0xe000 || scan <= 0xff {
                scan as u16
            } else {
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys_round_trip() {
        for (scan_code, expected) in [
            (0x1e, K | 0x04), // A
            (0x10, K | 0x14), // Q
            (0x2c, K | 0x1d), // Z
            (0x02, K | 0x1e), // 1
            (0x1c, K | 0x28), // Enter
            (0x39, K | 0x2c), // Space
            (0x0e, K | 0x2a), // Backspace
            (0x1d, K | 0xe0), // Left Ctrl
            (0x2a, K | 0xe1), // Left Shift
            (0x36, K | 0xe5), // Right Shift
            (0x38, K | 0xe2), // Left Alt
            (0x3a, K | 0x39), // Caps Lock
            (0x3b, K | 0x3a), // F1
            (0x58, K | 0x45), // F12
            (0x76, K | 0x73), // F24
            (0x56, K | 0x64), // ISO \
        ] {
            assert_eq!(usage(scan_code), Some(expected), "scan {scan_code:#x}");
            assert_eq!(scan(expected), Some(scan_code), "usage {expected:#x}");
        }
    }

    #[test]
    fn extended_keys_round_trip() {
        for (scan_code, expected) in [
            (0xe01d, K | 0xe4), // Right Ctrl
            (0xe038, K | 0xe6), // Right Alt
            (0xe05b, K | 0xe3), // Left Windows
            (0xe05d, K | 0x65), // Application
            (0xe047, K | 0x4a), // Home
            (0xe048, K | 0x52), // Up
            (0xe053, K | 0x4c), // Delete
            (0xe037, K | 0x46), // Print Screen
            (0xe045, K | 0x53), // Num Lock
            (0xe020, C | 0xe2), // Mute
            (0xe022, C | 0xcd), // Play/Pause
        ] {
            assert_eq!(usage(scan_code), Some(expected), "scan {scan_code:#x}");
            assert_eq!(scan(expected), Some(scan_code), "usage {expected:#x}");
        }
    }

    #[test]
    fn numpad_is_distinct_from_navigation() {
        for (keypad, navigation) in [
            (0x47, 0xe047), // 7 / Home
            (0x48, 0xe048), // 8 / Up
            (0x49, 0xe049), // 9 / Page Up
            (0x4b, 0xe04b), // 4 / Left
            (0x4d, 0xe04d), // 6 / Right
            (0x4f, 0xe04f), // 1 / End
            (0x50, 0xe050), // 2 / Down
            (0x51, 0xe051), // 3 / Page Down
            (0x52, 0xe052), // 0 / Insert
            (0x53, 0xe053), // . / Delete
            (0x1c, 0xe01c), // Enter / Keypad Enter
            (0x35, 0xe035), // / / Keypad /
        ] {
            let a = usage(keypad).unwrap();
            let b = usage(navigation).unwrap();
            assert_ne!(a, b);
            assert_eq!(scan(a), Some(keypad));
            assert_eq!(scan(b), Some(navigation));
        }
        for scan_code in 0x47..=0x53 {
            let key = usage(scan_code).unwrap();
            assert_eq!(scan(key), Some(scan_code));
        }
    }

    #[test]
    fn every_entry_is_stable() {
        for &(scan_code, key) in KEYS {
            assert!(matches!(key >> 16, 0x07 | 0x0c) && key & 0xffff != 0);
            assert!(scan(key).is_some(), "usage {key:#x}");
            // scan → usage → scan → usage reaches a fixed point.
            let resolved = usage(scan_code).unwrap();
            let injected = scan(resolved).unwrap();
            assert_eq!(usage(injected), Some(resolved), "scan {scan_code:#x}");
        }
    }

    #[test]
    fn aliases_map_to_primary_keys() {
        assert_eq!(usage(0x54), Some(K | 0x46));
        assert_eq!(usage(0xe046), Some(K | 0x48));
        assert_eq!(scan(K | 0x32), Some(0x2b));
        assert_eq!(scan(K | 0x46), Some(0xe037));
        assert_eq!(scan(K | 0x48), Some(0x45));
    }

    #[cfg(windows)]
    #[test]
    fn fixed_virtual_keys_win_over_scan_codes() {
        assert_eq!(observed(0x13, 0x45), Some(K | 0x48));
        assert_eq!(observed(0x90, 0xe045), Some(K | 0x53));
        assert_eq!(observed(0x90, 0x45), Some(K | 0x53));
        assert_eq!(observed(0xaf, 0), Some(C | 0xe9));
        assert_eq!(observed(0x41, 0x1e), Some(K | 0x04));
        assert_eq!(observed(0x24, 0x47), Some(K | 0x5f));
        assert_eq!(observed(0x24, 0xe047), Some(K | 0x4a));
        // Hardware fake shifts (E0 2A) never reach the host.
        assert_eq!(observed(0xa0, 0xe02a), None);
    }

    #[test]
    fn locks_and_modifiers() {
        assert_eq!(lock_index(usage(0x3a).unwrap()), Some(0));
        assert_eq!(lock_index(usage(0xe045).unwrap()), Some(1));
        assert_eq!(lock_index(usage(0x46).unwrap()), Some(2));
        assert_eq!(lock_index(K | 0x04), None);
        assert!(modifier(usage(0xe05c).unwrap()));
        assert!(modifier(usage(0x2a).unwrap()));
        assert!(!modifier(usage(0x39).unwrap()));
    }
}
