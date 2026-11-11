//! Keys travel as USB HID usage codes (keyboard page 0x07): physical key
//! positions, independent of layout. The controlled computer's own layout
//! and IME turn them into characters.
//!
//! Each row: HID usage, Windows set-1 scan code, whether Windows marks it
//! extended (E0), macOS virtual key code (`kVK_*`, `None` if the Mac has no
//! such key).

use super::protocol::Os;

const ROWS: &[(u16, u16, bool, Option<u16>)] = &[
    // Letters
    (0x04, 0x1E, false, Some(0x00)), // A
    (0x05, 0x30, false, Some(0x0B)), // B
    (0x06, 0x2E, false, Some(0x08)), // C
    (0x07, 0x20, false, Some(0x02)), // D
    (0x08, 0x12, false, Some(0x0E)), // E
    (0x09, 0x21, false, Some(0x03)), // F
    (0x0A, 0x22, false, Some(0x05)), // G
    (0x0B, 0x23, false, Some(0x04)), // H
    (0x0C, 0x17, false, Some(0x22)), // I
    (0x0D, 0x24, false, Some(0x26)), // J
    (0x0E, 0x25, false, Some(0x28)), // K
    (0x0F, 0x26, false, Some(0x25)), // L
    (0x10, 0x32, false, Some(0x2E)), // M
    (0x11, 0x31, false, Some(0x2D)), // N
    (0x12, 0x18, false, Some(0x1F)), // O
    (0x13, 0x19, false, Some(0x23)), // P
    (0x14, 0x10, false, Some(0x0C)), // Q
    (0x15, 0x13, false, Some(0x0F)), // R
    (0x16, 0x1F, false, Some(0x01)), // S
    (0x17, 0x14, false, Some(0x11)), // T
    (0x18, 0x16, false, Some(0x20)), // U
    (0x19, 0x2F, false, Some(0x09)), // V
    (0x1A, 0x11, false, Some(0x0D)), // W
    (0x1B, 0x2D, false, Some(0x07)), // X
    (0x1C, 0x15, false, Some(0x10)), // Y
    (0x1D, 0x2C, false, Some(0x06)), // Z
    // Digits
    (0x1E, 0x02, false, Some(0x12)), // 1
    (0x1F, 0x03, false, Some(0x13)), // 2
    (0x20, 0x04, false, Some(0x14)), // 3
    (0x21, 0x05, false, Some(0x15)), // 4
    (0x22, 0x06, false, Some(0x17)), // 5
    (0x23, 0x07, false, Some(0x16)), // 6
    (0x24, 0x08, false, Some(0x1A)), // 7
    (0x25, 0x09, false, Some(0x1C)), // 8
    (0x26, 0x0A, false, Some(0x19)), // 9
    (0x27, 0x0B, false, Some(0x1D)), // 0
    // Editing and punctuation
    (0x28, 0x1C, false, Some(0x24)), // Enter
    (0x29, 0x01, false, Some(0x35)), // Escape
    (0x2A, 0x0E, false, Some(0x33)), // Backspace
    (0x2B, 0x0F, false, Some(0x30)), // Tab
    (0x2C, 0x39, false, Some(0x31)), // Space
    (0x2D, 0x0C, false, Some(0x1B)), // -
    (0x2E, 0x0D, false, Some(0x18)), // =
    (0x2F, 0x1A, false, Some(0x21)), // [
    (0x30, 0x1B, false, Some(0x1E)), // ]
    (0x31, 0x2B, false, Some(0x2A)), // \
    (0x33, 0x27, false, Some(0x29)), // ;
    (0x34, 0x28, false, Some(0x27)), // '
    (0x35, 0x29, false, Some(0x32)), // `
    (0x36, 0x33, false, Some(0x2B)), // ,
    (0x37, 0x34, false, Some(0x2F)), // .
    (0x38, 0x35, false, Some(0x2C)), // /
    (0x39, 0x3A, false, Some(0x39)), // Caps Lock
    // Function keys
    (0x3A, 0x3B, false, Some(0x7A)), // F1
    (0x3B, 0x3C, false, Some(0x78)), // F2
    (0x3C, 0x3D, false, Some(0x63)), // F3
    (0x3D, 0x3E, false, Some(0x76)), // F4
    (0x3E, 0x3F, false, Some(0x60)), // F5
    (0x3F, 0x40, false, Some(0x61)), // F6
    (0x40, 0x41, false, Some(0x62)), // F7
    (0x41, 0x42, false, Some(0x64)), // F8
    (0x42, 0x43, false, Some(0x65)), // F9
    (0x43, 0x44, false, Some(0x6D)), // F10
    (0x44, 0x57, false, Some(0x67)), // F11
    (0x45, 0x58, false, Some(0x6F)), // F12
    (0x68, 0x64, false, Some(0x69)), // F13
    (0x69, 0x65, false, Some(0x6B)), // F14
    (0x6A, 0x66, false, Some(0x71)), // F15
    (0x6B, 0x67, false, Some(0x6A)), // F16
    (0x6C, 0x68, false, Some(0x40)), // F17
    (0x6D, 0x69, false, Some(0x4F)), // F18
    (0x6E, 0x6A, false, Some(0x50)), // F19
    (0x6F, 0x6B, false, Some(0x5A)), // F20
    // Navigation
    (0x46, 0x37, true, None),        // Print Screen
    (0x47, 0x46, false, None),       // Scroll Lock
    (0x48, 0x45, false, None),       // Pause (see `is_pause`)
    (0x49, 0x52, true, Some(0x72)),  // Insert (Mac: Help)
    (0x4A, 0x47, true, Some(0x73)),  // Home
    (0x4B, 0x49, true, Some(0x74)),  // Page Up
    (0x4C, 0x53, true, Some(0x75)),  // Delete (forward)
    (0x4D, 0x4F, true, Some(0x77)),  // End
    (0x4E, 0x51, true, Some(0x79)),  // Page Down
    (0x4F, 0x4D, true, Some(0x7C)),  // Right
    (0x50, 0x4B, true, Some(0x7B)),  // Left
    (0x51, 0x50, true, Some(0x7D)),  // Down
    (0x52, 0x48, true, Some(0x7E)),  // Up
    // Keypad
    (0x53, 0x45, true, Some(0x47)),  // Num Lock (Mac: Clear)
    (0x54, 0x35, true, Some(0x4B)),  // KP /
    (0x55, 0x37, false, Some(0x43)), // KP *
    (0x56, 0x4A, false, Some(0x4E)), // KP -
    (0x57, 0x4E, false, Some(0x45)), // KP +
    (0x58, 0x1C, true, Some(0x4C)),  // KP Enter
    (0x59, 0x4F, false, Some(0x53)), // KP 1
    (0x5A, 0x50, false, Some(0x54)), // KP 2
    (0x5B, 0x51, false, Some(0x55)), // KP 3
    (0x5C, 0x4B, false, Some(0x56)), // KP 4
    (0x5D, 0x4C, false, Some(0x57)), // KP 5
    (0x5E, 0x4D, false, Some(0x58)), // KP 6
    (0x5F, 0x47, false, Some(0x59)), // KP 7
    (0x60, 0x48, false, Some(0x5B)), // KP 8
    (0x61, 0x49, false, Some(0x5C)), // KP 9
    (0x62, 0x52, false, Some(0x52)), // KP 0
    (0x63, 0x53, false, Some(0x41)), // KP .
    (0x67, 0x59, false, Some(0x51)), // KP =
    // International and extra keys
    (0x64, 0x56, false, Some(0x0A)), // ISO key between Shift and Z (Mac: §/`)
    (0x65, 0x5D, true, Some(0x6E)),  // Menu / Application
    (0x87, 0x73, false, Some(0x5E)), // International1 (JIS Ro / _)
    (0x88, 0x70, false, Some(0x68)), // International2 (Kana)
    (0x89, 0x7D, false, Some(0x5D)), // International3 (Yen)
    (0x8A, 0x79, false, None),       // International4 (Henkan)
    (0x8B, 0x7B, false, None),       // International5 (Muhenkan)
    (0x90, 0x72, false, Some(0x68)), // LANG1 (Hangul / Kana)
    (0x91, 0x71, false, Some(0x66)), // LANG2 (Hanja / Eisu)
    // Media
    (0x7F, 0x20, true, Some(0x4A)),  // Mute
    (0x80, 0x30, true, Some(0x48)),  // Volume Up
    (0x81, 0x2E, true, Some(0x49)),  // Volume Down
    // Modifiers
    (0xE0, 0x1D, false, Some(0x3B)), // Left Ctrl
    (0xE1, 0x2A, false, Some(0x38)), // Left Shift
    (0xE2, 0x38, false, Some(0x3A)), // Left Alt / Option
    (0xE3, 0x5B, true, Some(0x37)),  // Left Win / Command
    (0xE4, 0x1D, true, Some(0x3E)),  // Right Ctrl
    (0xE5, 0x36, false, Some(0x3C)), // Right Shift
    (0xE6, 0x38, true, Some(0x3D)),  // Right Alt / Option (AltGr)
    (0xE7, 0x5C, true, Some(0x36)),  // Right Win / Command
];

pub const HID_ESCAPE: u16 = 0x29;
pub const HID_RIGHT: u16 = 0x4F;
pub const HID_LEFT: u16 = 0x50;
pub const HID_DOWN: u16 = 0x51;
pub const HID_UP: u16 = 0x52;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const HID_PAUSE: u16 = 0x48;
pub const HID_LCTRL: u16 = 0xE0;
pub const HID_LSHIFT: u16 = 0xE1;
pub const HID_LALT: u16 = 0xE2;
pub const HID_LGUI: u16 = 0xE3;
pub const HID_RCTRL: u16 = 0xE4;
pub const HID_RSHIFT: u16 = 0xE5;
pub const HID_RALT: u16 = 0xE6;
pub const HID_RGUI: u16 = 0xE7;

pub fn is_modifier(hid: u16) -> bool {
    (HID_LCTRL..=HID_RGUI).contains(&hid)
}

/// Windows reports Pause as scan 0x45 without the extended flag, and Num
/// Lock as 0x45 with it; Pause must be injected by virtual key.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn is_pause(hid: u16) -> bool {
    hid == HID_PAUSE
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn hid_from_windows(scan: u16, extended: bool) -> Option<u16> {
    ROWS.iter().find(|r| r.1 == scan && r.2 == extended).map(|r| r.0)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn windows_from_hid(hid: u16) -> Option<(u16, bool)> {
    ROWS.iter().find(|r| r.0 == hid).map(|r| (r.1, r.2))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn hid_from_mac(keycode: u16) -> Option<u16> {
    // 0x68 is both International2 and LANG1 on a Mac (Kana); prefer LANG1.
    if keycode == 0x68 {
        return Some(0x90);
    }
    ROWS.iter().find(|r| r.3 == Some(keycode)).map(|r| r.0)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn mac_from_hid(hid: u16) -> Option<u16> {
    ROWS.iter().find(|r| r.0 == hid).and_then(|r| r.3)
}

/// "Ctrl acts as ⌘ on a Mac": on a PC keyboard controlling a Mac, Ctrl
/// becomes Command and the Windows key becomes Control, so Ctrl+C copies.
/// On a Mac keyboard controlling a PC, Command becomes Ctrl and Control
/// becomes the Windows key. Alt/Option and everything else stay put.
pub fn map_modifier(hid: u16, from: Os, to: Os, swap_ctrl_cmd: bool) -> u16 {
    if !swap_ctrl_cmd || from == to {
        return hid;
    }
    match hid {
        HID_LCTRL => HID_LGUI,
        HID_LGUI => HID_LCTRL,
        HID_RCTRL => HID_RGUI,
        HID_RGUI => HID_RCTRL,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_is_unique_per_platform_and_round_trips() {
        let mut seen_hid = std::collections::HashSet::new();
        let mut seen_win = std::collections::HashSet::new();
        for &(hid, scan, ext, mac) in ROWS {
            assert!(seen_hid.insert(hid), "duplicate HID {hid:#x}");
            assert!(seen_win.insert((scan, ext)), "duplicate Windows scan {scan:#x}/{ext}");
            assert_eq!(hid_from_windows(scan, ext), Some(hid));
            assert_eq!(windows_from_hid(hid), Some((scan, ext)));
            if let Some(code) = mac {
                assert_eq!(mac_from_hid(hid), Some(code));
                if code != 0x68 {
                    assert_eq!(hid_from_mac(code), Some(hid), "mac {code:#x}");
                }
            }
        }
    }

    #[test]
    fn letters_land_on_the_same_physical_key() {
        // Windows Q (scan 0x10) is Mac kVK_ANSI_Q (0x0C).
        let hid = hid_from_windows(0x10, false).unwrap();
        assert_eq!(mac_from_hid(hid), Some(0x0C));
        // Mac kVK_ANSI_A (0) is Windows scan 0x1E.
        assert_eq!(windows_from_hid(hid_from_mac(0x00).unwrap()), Some((0x1E, false)));
    }

    #[test]
    fn extended_flag_separates_keypad_from_navigation() {
        assert_eq!(hid_from_windows(0x48, true), Some(HID_UP));
        assert_eq!(hid_from_windows(0x48, false), Some(0x60)); // KP 8
        assert_eq!(hid_from_windows(0x1D, false), Some(HID_LCTRL));
        assert_eq!(hid_from_windows(0x1D, true), Some(HID_RCTRL));
    }

    #[test]
    fn ctrl_and_command_swap_only_across_systems_when_enabled() {
        assert_eq!(map_modifier(HID_LCTRL, Os::Windows, Os::MacOs, true), HID_LGUI);
        assert_eq!(map_modifier(HID_LGUI, Os::Windows, Os::MacOs, true), HID_LCTRL);
        assert_eq!(map_modifier(HID_RGUI, Os::MacOs, Os::Windows, true), HID_RCTRL);
        assert_eq!(map_modifier(HID_LALT, Os::Windows, Os::MacOs, true), HID_LALT);
        assert_eq!(map_modifier(HID_LCTRL, Os::Windows, Os::MacOs, false), HID_LCTRL);
        assert_eq!(map_modifier(HID_LCTRL, Os::Windows, Os::Windows, true), HID_LCTRL);
        assert_eq!(map_modifier(0x06, Os::Windows, Os::MacOs, true), 0x06);
    }
}
