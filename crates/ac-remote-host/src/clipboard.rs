//! The shared PC's clipboard (text), for a viewer that asks for it: the
//! phone app copies and pastes through it. Linux uses wl-clipboard
//! (`wl-paste --watch`, `wl-copy`); Windows the Win32 clipboard.

use anyhow::Result;
use tokio::sync::mpsc;

pub use imp::{set, Watch};

/// Watches the clipboard: its text now, then each time it changes, goes to
/// `tx` until the returned [`Watch`] is dropped.
pub fn watch(tx: mpsc::UnboundedSender<String>) -> Result<Watch> {
    imp::watch(tx)
}

#[cfg(unix)]
mod imp {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, Stdio};

    use anyhow::{bail, Context, Result};
    use tokio::sync::mpsc;

    pub struct Watch(Child);

    impl Drop for Watch {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn read() -> Option<String> {
        let out = Command::new("wl-paste").args(["--no-newline", "--type", "text"]).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn watch(tx: mpsc::UnboundedSender<String>) -> Result<Watch> {
        // One line per change (and one for the clipboard as it is now).
        let mut child = Command::new("wl-paste")
            .args(["--watch", "echo"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("running wl-paste (install wl-clipboard to share the clipboard)")?;
        let out = child.stdout.take().context("wl-paste output")?;
        std::thread::Builder::new().name("share-clipboard".into()).spawn(move || {
            for line in BufReader::new(out).lines() {
                if line.is_err() {
                    break;
                }
                if let Some(text) = read().filter(|t| !t.is_empty()) {
                    if tx.send(text).is_err() {
                        break;
                    }
                }
            }
        })?;
        Ok(Watch(child))
    }

    pub fn set(text: &str) -> Result<()> {
        // wl-copy forks to keep serving the text and the parent exits at once;
        // the fork keeps any pipe it inherits open, so none are given.
        let mut child = Command::new("wl-copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("running wl-copy (install wl-clipboard to share the clipboard)")?;
        child.stdin.take().context("wl-copy input")?.write_all(text.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            bail!("wl-copy failed ({status})");
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tokio::sync::mpsc;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
        SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    const POLL: Duration = Duration::from_millis(300);

    pub struct Watch(Arc<AtomicBool>);

    impl Drop for Watch {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// Another program may hold the clipboard for a moment: try a few times.
    fn open() -> Result<()> {
        for _ in 0..10 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe { OpenClipboard(None) }.context("opening the clipboard")
    }

    fn read() -> Option<String> {
        let format = CF_UNICODETEXT.0 as u32;
        unsafe { IsClipboardFormatAvailable(format) }.ok()?;
        open().ok()?;
        let text = (|| unsafe {
            let h = GetClipboardData(format).ok()?;
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u16;
            if p.is_null() {
                return None;
            }
            let max = GlobalSize(g) / 2;
            let s = std::slice::from_raw_parts(p, max);
            let len = s.iter().position(|&c| c == 0).unwrap_or(max);
            let text = String::from_utf16_lossy(&s[..len]);
            let _ = GlobalUnlock(g);
            Some(text)
        })();
        let _ = unsafe { CloseClipboard() };
        text
    }

    pub fn watch(tx: mpsc::UnboundedSender<String>) -> Result<Watch> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::Builder::new().name("share-clipboard".into()).spawn(move || {
            let mut seen = 0u32;
            while !stopped.load(Ordering::Relaxed) && !tx.is_closed() {
                let seq = unsafe { GetClipboardSequenceNumber() };
                if seq != seen {
                    seen = seq;
                    if let Some(text) = read().filter(|t| !t.is_empty()) {
                        if tx.send(text).is_err() {
                            break;
                        }
                    }
                }
                std::thread::sleep(POLL);
            }
        })?;
        Ok(Watch(stop))
    }

    pub fn set(text: &str) -> Result<()> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        open()?;
        let r = unsafe {
            (|| -> Result<()> {
                EmptyClipboard()?;
                let g = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
                let p = GlobalLock(g) as *mut u16;
                if p.is_null() {
                    anyhow::bail!("locking clipboard memory");
                }
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(g);
                // The clipboard owns the memory from here on.
                SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(g.0)))?;
                Ok(())
            })()
        };
        let _ = unsafe { CloseClipboard() };
        r
    }
}

#[cfg(test)]
mod tests {
    /// Uses the real clipboard (and puts it back): `cargo test -- --ignored clipboard`.
    #[test]
    #[ignore]
    fn sets_and_watches_the_clipboard() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let before = std::process::Command::new("wl-paste").arg("--no-newline").output().unwrap().stdout;
        let w = super::watch(tx).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        while rx.try_recv().is_ok() {}
        super::set("Test Bed clipboard check ✓").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut got = None;
        while got.is_none() && std::time::Instant::now() < deadline {
            got = rx.try_recv().ok();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        drop(w);
        super::set(&String::from_utf8_lossy(&before)).unwrap();
        assert_eq!(got.as_deref(), Some("Test Bed clipboard check ✓"));
    }
}
