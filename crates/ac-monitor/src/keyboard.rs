//! Typing on the PC from the phone's on-screen keyboard, through
//! `zwp_virtual_keyboard_v1`. Like `wtype`, the keymap is made from the text
//! itself: one key per character, so no modifiers and no layout to match.

use std::io::Write;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1 as manager_v1, zwp_virtual_keyboard_v1 as keyboard_v1,
};

/// Keys the phone may name, as XKB keysym names.
pub const NAMED_KEYS: &[&str] = &["BackSpace", "Return", "Tab", "Escape", "Delete", "Left", "Right", "Up", "Down", "Home", "End"];
/// A keymap that grew past this starts over.
const MAX_KEYS: usize = 200;
const KEY_GAP: Duration = Duration::from_millis(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyInput {
    Text(String),
    /// One of [`NAMED_KEYS`].
    Key(String),
}

struct State;

pub struct VirtualKeyboard {
    conn: Connection,
    queue: EventQueue<State>,
    keyboard: keyboard_v1::ZwpVirtualKeyboardV1,
    /// Keysym names; key `i` has the evdev code `i + 1`.
    keys: Vec<String>,
    uploaded: usize,
    started: Instant,
}

/// The keysym name typing `c` needs.
pub fn keysym_name(c: char) -> String {
    match c {
        '\n' | '\r' => "Return".into(),
        '\t' => "Tab".into(),
        ' ' => "space".into(),
        c if c.is_ascii_alphanumeric() => c.to_string(),
        c => format!("U{:04X}", c as u32),
    }
}

/// An XKB keymap with one single-level key per name.
pub fn keymap(keys: &[String]) -> String {
    let mut codes = String::new();
    let mut symbols = String::new();
    for (i, k) in keys.iter().enumerate() {
        codes.push_str(&format!("    <K{i}> = {};\n", i + 9));
        symbols.push_str(&format!("    key <K{i}> {{ [ {k} ] }};\n"));
    }
    format!(
        "xkb_keymap {{\n  xkb_keycodes \"tb\" {{\n    minimum = 8;\n    maximum = {};\n{codes}  }};\n  \
xkb_types \"tb\" {{ include \"complete\" }};\n  xkb_compatibility \"tb\" {{ include \"complete\" }};\n  \
xkb_symbols \"tb\" {{\n{symbols}  }};\n}};\n",
        keys.len() + 9
    )
}

impl VirtualKeyboard {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let (globals, queue) = registry_queue_init::<State>(&conn).context("reading Wayland globals")?;
        let qh = queue.handle();
        let manager = globals
            .bind::<manager_v1::ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
            .context("this compositor has no virtual keyboard")?;
        let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=1, ()).context("no Wayland seat")?;
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        Ok(Self { conn, queue, keyboard, keys: Vec::new(), uploaded: 0, started: Instant::now() })
    }

    fn code_for(&mut self, name: &str) -> u32 {
        let i = match self.keys.iter().position(|k| k == name) {
            Some(i) => i,
            None => {
                if self.keys.len() >= MAX_KEYS {
                    self.keys.clear();
                    self.uploaded = 0;
                }
                self.keys.push(name.to_owned());
                self.keys.len() - 1
            }
        };
        i as u32 + 1
    }

    /// Sends the keymap if keys were added since the last one.
    fn upload(&mut self) -> Result<()> {
        if self.uploaded == self.keys.len() && self.uploaded > 0 {
            return Ok(());
        }
        let mut text = keymap(&self.keys).into_bytes();
        text.push(0);
        let fd = rustix::fs::memfd_create("test-bed-keymap", rustix::fs::MemfdFlags::CLOEXEC).context("memfd")?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&text).context("writing the keymap")?;
        self.keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1.into(), file.as_fd(), text.len() as u32);
        self.keyboard.modifiers(0, 0, 0, 0);
        self.uploaded = self.keys.len();
        self.queue.roundtrip(&mut State).context("Wayland roundtrip")?;
        Ok(())
    }

    fn tap(&mut self, code: u32) {
        let time = self.started.elapsed().as_millis() as u32;
        self.keyboard.key(time, code, wl_keyboard::KeyState::Pressed.into());
        self.keyboard.key(time, code, wl_keyboard::KeyState::Released.into());
    }

    pub fn apply(&mut self, input: &KeyInput) -> Result<()> {
        let names: Vec<String> = match input {
            KeyInput::Text(t) => t.chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t')).map(keysym_name).collect(),
            KeyInput::Key(k) if NAMED_KEYS.contains(&k.as_str()) => vec![k.clone()],
            KeyInput::Key(k) => return Err(anyhow!("unknown key {k}")),
        };
        let codes: Vec<u32> = names.iter().map(|n| self.code_for(n)).collect();
        self.upload()?;
        for code in codes {
            self.tap(code);
            self.conn.flush().context("Wayland flush")?;
            std::thread::sleep(KEY_GAP);
        }
        self.queue.dispatch_pending(&mut State).context("Wayland events")?;
        Ok(())
    }
}

impl Drop for VirtualKeyboard {
    fn drop(&mut self) {
        self.keyboard.destroy();
        let _ = self.conn.flush();
    }
}

/// Runs a keyboard on its own thread; dropping the sender ends it.
pub fn spawn() -> Result<std::sync::mpsc::Sender<KeyInput>> {
    let (tx, rx) = std::sync::mpsc::channel::<KeyInput>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new().name("keyboard".into()).spawn(move || {
        let mut keyboard = match VirtualKeyboard::connect() {
            Ok(k) => {
                let _ = ready_tx.send(Ok(()));
                k
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        for input in rx {
            if let Err(e) = keyboard.apply(&input) {
                tracing::warn!("keyboard: {e:#}");
            }
        }
    })?;
    ready_rx.recv().map_err(|_| anyhow!("the keyboard thread ended"))??;
    Ok(tx)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(wl_seat::WlSeat, manager_v1::ZwpVirtualKeyboardManagerV1, keyboard_v1::ZwpVirtualKeyboardV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_characters() {
        assert_eq!(keysym_name('a'), "a");
        assert_eq!(keysym_name('7'), "7");
        assert_eq!(keysym_name(' '), "space");
        assert_eq!(keysym_name('\n'), "Return");
        assert_eq!(keysym_name('!'), "U0021");
        assert_eq!(keysym_name('é'), "U00E9");
    }

    #[test]
    fn builds_a_keymap() {
        let k = keymap(&["a".into(), "BackSpace".into()]);
        assert!(k.contains("<K0> = 9;"));
        assert!(k.contains("key <K1> { [ BackSpace ] };"));
        assert!(k.contains("maximum = 11;"));
    }
}
