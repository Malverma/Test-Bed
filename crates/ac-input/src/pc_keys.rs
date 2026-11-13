//! Keys for the PC viewer, which sends Linux evdev codes (the shared PC
//! applies its own layout). GTK reports X11 keycodes on Linux (evdev + 8)
//! and Windows virtual-key codes on Windows.

/// Linux evdev codes for `A`..`Z`.
const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
/// Linux evdev codes for numpad `0`..`9`.
const NUMPAD: [u16; 10] = [82, 79, 80, 81, 75, 76, 77, 71, 72, 73];

/// Windows virtual-key code → Linux evdev code. Letters and punctuation are
/// the keys' US positions, so a non-US layout on the viewing PC can send a
/// neighbouring key (Windows reports the layout's meaning, not the position).
pub fn evdev_from_windows_vk(vk: u32) -> Option<u16> {
    Some(match vk {
        0x41..=0x5a => LETTERS[(vk - 0x41) as usize],
        0x30 => 11,
        0x31..=0x39 => (vk - 0x31 + 2) as u16,
        0x60..=0x69 => NUMPAD[(vk - 0x60) as usize],
        0x70..=0x79 => (vk - 0x70 + 59) as u16, // F1..F10
        0x7a => 87,
        0x7b => 88,
        0x7c..=0x87 => (vk - 0x7c + 183) as u16, // F13..F24
        0x08 => 14,  // BACK
        0x09 => 15,  // TAB
        0x0d => 28,  // RETURN (keypad Enter too)
        0x10 => 42,  // SHIFT
        0x11 => 29,  // CONTROL
        0x12 => 56,  // MENU (Alt)
        0x13 => 119, // PAUSE
        0x14 => 58,  // CAPITAL
        0x1b => 1,   // ESCAPE
        0x20 => 57,  // SPACE
        0x21 => 104, // PRIOR
        0x22 => 109, // NEXT
        0x23 => 107, // END
        0x24 => 102, // HOME
        0x25 => 105, // LEFT
        0x26 => 103, // UP
        0x27 => 106, // RIGHT
        0x28 => 108, // DOWN
        0x2c => 99,  // SNAPSHOT
        0x2d => 110, // INSERT
        0x2e => 111, // DELETE
        0x5b => 125, // LWIN
        0x5c => 126, // RWIN
        0x5d => 127, // APPS
        0x6a => 55,  // MULTIPLY
        0x6b => 78,  // ADD
        0x6d => 74,  // SUBTRACT
        0x6e => 83,  // DECIMAL
        0x6f => 98,  // DIVIDE
        0x90 => 69,  // NUMLOCK
        0x91 => 70,  // SCROLL
        0xa0 => 42,  // LSHIFT
        0xa1 => 54,  // RSHIFT
        0xa2 => 29,  // LCONTROL
        0xa3 => 97,  // RCONTROL
        0xa4 => 56,  // LMENU
        0xa5 => 100, // RMENU (AltGr)
        0xad => 113, // VOLUME_MUTE
        0xae => 114, // VOLUME_DOWN
        0xaf => 115, // VOLUME_UP
        0xb0 => 163, // MEDIA_NEXT_TRACK
        0xb1 => 165, // MEDIA_PREV_TRACK
        0xb2 => 166, // MEDIA_STOP
        0xb3 => 164, // MEDIA_PLAY_PAUSE
        0xba => 39,  // OEM_1 ;
        0xbb => 13,  // OEM_PLUS =
        0xbc => 51,  // OEM_COMMA
        0xbd => 12,  // OEM_MINUS
        0xbe => 52,  // OEM_PERIOD
        0xbf => 53,  // OEM_2 /
        0xc0 => 41,  // OEM_3 `
        0xdb => 26,  // OEM_4 [
        0xdc => 43,  // OEM_5 \
        0xdd => 27,  // OEM_6 ]
        0xde => 40,  // OEM_7 '
        0xe2 => 86,  // OEM_102
        _ => return None,
    })
}

/// The evdev code for a GTK hardware keycode on this platform.
pub fn evdev_from_gtk(keycode: u32) -> Option<u16> {
    if cfg!(windows) {
        evdev_from_windows_vk(keycode)
    } else {
        keycode.checked_sub(8).filter(|&c| c > 0).and_then(|c| u16::try_from(c).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_keys() {
        assert_eq!(evdev_from_windows_vk(0x41), Some(30)); // A
        assert_eq!(evdev_from_windows_vk(0x5a), Some(44)); // Z
        assert_eq!(evdev_from_windows_vk(0x31), Some(2)); // 1
        assert_eq!(evdev_from_windows_vk(0x30), Some(11)); // 0
        assert_eq!(evdev_from_windows_vk(0x7b), Some(88)); // F12
        assert_eq!(evdev_from_windows_vk(0x87), Some(194)); // F24
        assert_eq!(evdev_from_windows_vk(0x60), Some(82)); // numpad 0
        assert_eq!(evdev_from_windows_vk(0x5b), Some(125)); // Windows key
        assert_eq!(evdev_from_windows_vk(0xff), None);
    }

    #[test]
    fn linux_keycodes() {
        assert_eq!(evdev_from_gtk(38), if cfg!(windows) { None } else { Some(30) }); // A
        if !cfg!(windows) {
            assert_eq!(evdev_from_gtk(8), None);
            assert_eq!(evdev_from_gtk(3), None);
        }
    }
}
