//! Input injection on Windows with `SendInput`. Keys go in as scancodes, so
//! the shared PC's own keyboard layout applies, like a physical keyboard.
//! Linux evdev codes for the main block are AT set-1 scancodes already;
//! the rest are translated below.

use std::collections::HashSet;

use anyhow::{bail, Result};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};

use super::{is_valid_button, is_valid_key, map_abs, Target};

/// How a Linux key reaches Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WinKey {
    /// Set-1 scancode; `true` for the E0-prefixed (extended) keys.
    Scan(u16, bool),
    /// Keys without a stable scancode: media, volume, Pause.
    Vk(u16),
}

/// Linux evdev key code → Windows key.
fn win_key(code: u16) -> Option<WinKey> {
    use WinKey::*;
    Some(match code {
        // ESC..KP_DOT, then 102ND, F11, F12: the scancode is the evdev code.
        1..=83 | 86..=88 => Scan(code, false),
        96 => Scan(0x1c, true),  // KP_ENTER
        97 => Scan(0x1d, true),  // RIGHTCTRL
        98 => Scan(0x35, true),  // KP_SLASH
        99 => Scan(0x37, true),  // SYSRQ (Print Screen)
        100 => Scan(0x38, true), // RIGHTALT
        102 => Scan(0x47, true), // HOME
        103 => Scan(0x48, true), // UP
        104 => Scan(0x49, true), // PAGEUP
        105 => Scan(0x4b, true), // LEFT
        106 => Scan(0x4d, true), // RIGHT
        107 => Scan(0x4f, true), // END
        108 => Scan(0x50, true), // DOWN
        109 => Scan(0x51, true), // PAGEDOWN
        110 => Scan(0x52, true), // INSERT
        111 => Scan(0x53, true), // DELETE
        117 => Scan(0x59, false), // KP_EQUAL
        119 => Vk(0x13),          // PAUSE
        121 => Scan(0x53, false), // KP_COMMA → keypad decimal
        125 => Scan(0x5b, true),  // LEFTMETA (Windows key)
        126 => Scan(0x5c, true),  // RIGHTMETA
        127 => Scan(0x5d, true),  // COMPOSE (Menu)
        113 => Vk(0xad),          // MUTE
        114 => Vk(0xae),          // VOLUMEDOWN
        115 => Vk(0xaf),          // VOLUMEUP
        163 => Vk(0xb0),          // NEXTSONG
        164 => Vk(0xb3),          // PLAYPAUSE
        165 => Vk(0xb1),          // PREVIOUSSONG
        166 => Vk(0xb2),          // STOPCD
        183..=194 => Vk(0x7c + (code - 183)), // F13..F24
        _ => return None,
    })
}

pub struct Injector {
    keys_down: HashSet<u16>,
    buttons_down: HashSet<u16>,
    target: Option<Target>,
    last_motion_seq: Option<u32>,
    rel_rem: (f32, f32),
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // Blocked by UIPI: the foreground window runs as administrator.
        bail!("Windows refused the input (an administrator window has focus?)");
    }
    Ok(())
}

fn key_input(k: WinKey, pressed: bool) -> INPUT {
    let up = if pressed { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    let ki = match k {
        WinKey::Scan(sc, ext) => KEYBDINPUT {
            wVk: VIRTUAL_KEY(0),
            wScan: sc,
            dwFlags: KEYEVENTF_SCANCODE | up | if ext { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) },
            time: 0,
            dwExtraInfo: 0,
        },
        WinKey::Vk(vk) => KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: up, time: 0, dwExtraInfo: 0 },
    };
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

/// Linux button code → (flags for press, flags for release, X button number).
fn button_flags(code: u16) -> Option<(MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS, i32)> {
    Some(match code {
        0x110 => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        0x111 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        0x112 => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        0x113 | 0x116 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1), // SIDE, BACK
        0x114 | 0x115 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2), // EXTRA, FORWARD
        _ => return None,
    })
}

impl Injector {
    pub fn new() -> Result<Self> {
        Ok(Self { keys_down: HashSet::new(), buttons_down: HashSet::new(), target: None, last_motion_seq: None, rel_rem: (0.0, 0.0) })
    }

    pub fn set_target(&mut self, t: Target) {
        self.target = Some(t);
    }

    /// Reset per-connection state (sequence numbers).
    pub fn new_session(&mut self) {
        self.last_motion_seq = None;
        self.release_all();
    }

    pub fn key(&mut self, code: u16, pressed: bool) -> Result<()> {
        let Some(k) = win_key(code).filter(|_| is_valid_key(code)) else { return Ok(()) };
        let changed = if pressed { self.keys_down.insert(code) } else { self.keys_down.remove(&code) };
        if changed {
            send(&[key_input(k, pressed)])?;
        }
        Ok(())
    }

    pub fn button(&mut self, code: u16, pressed: bool) -> Result<()> {
        let Some((down, up, x)) = button_flags(code).filter(|_| is_valid_button(code)) else { return Ok(()) };
        let changed = if pressed { self.buttons_down.insert(code) } else { self.buttons_down.remove(&code) };
        if changed {
            send(&[mouse_input(0, 0, x, if pressed { down } else { up })])?;
        }
        Ok(())
    }

    /// Scroll in 1/120 notch units; positive y = scroll down. Windows' wheel
    /// delta is 120 per notch too, positive = up.
    pub fn scroll(&mut self, v120_x: i32, v120_y: i32) -> Result<()> {
        let mut ev = Vec::with_capacity(2);
        if v120_y != 0 {
            ev.push(mouse_input(0, 0, -v120_y, MOUSEEVENTF_WHEEL));
        }
        if v120_x != 0 {
            ev.push(mouse_input(0, 0, v120_x, MOUSEEVENTF_HWHEEL));
        }
        if !ev.is_empty() {
            send(&ev)?;
        }
        Ok(())
    }

    fn accept_seq(&mut self, seq: u32) -> bool {
        if let Some(last) = self.last_motion_seq {
            if (seq.wrapping_sub(last) as i32) <= 0 {
                return false;
            }
        }
        self.last_motion_seq = Some(seq);
        true
    }

    /// Absolute motion, normalised 0..1 within the streamed monitor. The
    /// layout box is the virtual desktop, which `MOUSEEVENTF_VIRTUALDESK`
    /// spreads 0..65535 across, like uinput's absolute axes.
    pub fn motion_abs(&mut self, seq: u32, x: f32, y: f32) -> Result<()> {
        if !self.accept_seq(seq) {
            return Ok(());
        }
        let Some(t) = self.target else { return Ok(()) };
        let (ax, ay) = map_abs(&t, x, y);
        send(&[mouse_input(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)])
    }

    pub fn motion_rel(&mut self, seq: u32, dx: f32, dy: f32) -> Result<()> {
        if !self.accept_seq(seq) {
            return Ok(());
        }
        let fx = dx + self.rel_rem.0;
        let fy = dy + self.rel_rem.1;
        let (ix, iy) = (fx.trunc() as i32, fy.trunc() as i32);
        self.rel_rem = (fx - ix as f32, fy - iy as f32);
        if ix != 0 || iy != 0 {
            send(&[mouse_input(ix, iy, 0, MOUSEEVENTF_MOVE)])?;
        }
        Ok(())
    }

    /// Release every key and button currently held.
    pub fn release_all(&mut self) {
        let mut ev: Vec<INPUT> = self.keys_down.drain().filter_map(|k| win_key(k).map(|w| key_input(w, false))).collect();
        ev.extend(self.buttons_down.drain().filter_map(|b| button_flags(b).map(|(_, up, x)| mouse_input(0, 0, x, up))));
        if !ev.is_empty() {
            if let Err(e) = send(&ev) {
                tracing::warn!("releasing keys: {e}");
            }
        }
        self.rel_rem = (0.0, 0.0);
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_scancodes() {
        assert_eq!(win_key(30), Some(WinKey::Scan(0x1e, false))); // A
        assert_eq!(win_key(28), Some(WinKey::Scan(0x1c, false))); // Enter
        assert_eq!(win_key(96), Some(WinKey::Scan(0x1c, true))); // keypad Enter
        assert_eq!(win_key(125), Some(WinKey::Scan(0x5b, true))); // Windows key
        assert_eq!(win_key(88), Some(WinKey::Scan(0x58, false))); // F12
        assert_eq!(win_key(194), Some(WinKey::Vk(0x87))); // F24
        assert_eq!(win_key(84), None);
    }
}
