//! Notices someone using this PC's own keyboard, mouse or touchpad. They
//! come first: while they are active, the remote PC's input is dropped (see
//! `server.rs`). Input Test Bed injects itself is never counted: on Linux it
//! comes from our own virtual devices, which aren't watched, and on Windows
//! it carries the "injected" flag.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

/// Remote input resumes this long after the last local input.
pub const HOLD: Duration = Duration::from_millis(1500);

static START: OnceLock<Instant> = OnceLock::new();
/// Milliseconds after `START` of the last local input, plus one; 0 = never.
static LAST: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

fn touch() {
    LAST.store(now_ms(), Ordering::Relaxed);
}

/// Whether someone used this PC's own input within [`HOLD`].
pub fn active() -> bool {
    let last = LAST.load(Ordering::Relaxed);
    last != 0 && now_ms().saturating_sub(last) < HOLD.as_millis() as u64
}

/// Starts watching, once. Failures are logged: remote input then always goes through.
pub fn start() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        START.get_or_init(Instant::now);
        if let Err(e) = imp::start() {
            tracing::warn!("can't watch this PC's own keyboard and mouse, so it can't take over from a remote PC: {e:#}");
        }
    });
}

#[cfg(unix)]
mod imp {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{bail, Result};
    use evdev::{Device, EventType, KeyCode, PropType};

    /// How often new devices (a mouse plugged in) are looked for.
    const RESCAN: Duration = Duration::from_secs(3);

    /// Keyboards, mice, touchpads and the like; not our own virtual devices,
    /// nor sensors that report all the time.
    fn wanted(dev: &Device) -> bool {
        let name = dev.name().unwrap_or("");
        if name.starts_with("Test Bed") || dev.properties().contains(PropType::ACCELEROMETER) {
            return false;
        }
        dev.supported_keys().is_some_and(|k| k.iter().next().is_some()) || dev.supported_relative_axes().is_some()
    }

    /// Absolute axes count only on touch devices: a gamepad's sticks drift.
    fn counts_abs(dev: &Device) -> bool {
        dev.supported_keys().is_some_and(|k| k.contains(KeyCode::BTN_TOUCH) || k.contains(KeyCode::BTN_TOOL_PEN))
    }

    fn watch(path: PathBuf, mut dev: Device, watched: Arc<Mutex<HashSet<PathBuf>>>) {
        let abs = counts_abs(&dev);
        let name = path.display().to_string();
        let r = std::thread::Builder::new().name("local-input".into()).spawn({
            let watched = watched.clone();
            let path = path.clone();
            move || {
                while let Ok(events) = dev.fetch_events() {
                    if events.into_iter().any(|e| {
                        let t = e.event_type();
                        t == EventType::KEY || t == EventType::RELATIVE || (abs && t == EventType::ABSOLUTE)
                    }) {
                        super::touch();
                    }
                }
                // Unplugged.
                watched.lock().unwrap().remove(&path);
            }
        });
        if let Err(e) = r {
            tracing::warn!("watching {name}: {e}");
            watched.lock().unwrap().remove(&path);
        }
    }

    fn scan(watched: &Arc<Mutex<HashSet<PathBuf>>>) -> usize {
        let mut found = 0;
        for (path, dev) in evdev::enumerate() {
            if !wanted(&dev) {
                continue;
            }
            found += 1;
            if watched.lock().unwrap().insert(path.clone()) {
                watch(path, dev, watched.clone());
            }
        }
        found
    }

    pub fn start() -> Result<()> {
        let watched = Arc::new(Mutex::new(HashSet::new()));
        if scan(&watched) == 0 {
            bail!("no readable input devices (use Finish setup in Test Bed's PC tab)");
        }
        std::thread::Builder::new().name("local-input-scan".into()).spawn(move || loop {
            std::thread::sleep(RESCAN);
            scan(&watched);
        })?;
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, SetWindowsHookExW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
        MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    unsafe extern "system" fn keyboard(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let k = unsafe { &*(l.0 as *const KBDLLHOOKSTRUCT) };
            if (k.flags & LLKHF_INJECTED).0 == 0 {
                super::touch();
            }
        }
        unsafe { CallNextHookEx(None, code, w, l) }
    }

    unsafe extern "system" fn mouse(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let m = unsafe { &*(l.0 as *const MSLLHOOKSTRUCT) };
            if m.flags & LLMHF_INJECTED == 0 {
                super::touch();
            }
        }
        unsafe { CallNextHookEx(None, code, w, l) }
    }

    /// Low-level hooks run on the thread that set them, inside its message
    /// loop; they only note the time, so they never slow input down.
    pub fn start() -> Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new().name("local-input".into()).spawn(move || {
            let hooks = unsafe {
                GetModuleHandleW(None).and_then(|m| {
                    let k = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), Some(m.into()), 0)?;
                    let p = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), Some(m.into()), 0)?;
                    Ok((k, p))
                })
            };
            let ok = hooks.is_ok();
            let _ = tx.send(hooks.map(|_| ()));
            if ok {
                let mut msg = MSG::default();
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {}
            }
        })?;
        rx.recv().context("input hook thread")?.context("setting the input hooks")
    }
}
