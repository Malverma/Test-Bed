//! Where a file sent to this PC goes: the folder open where it was dropped
//! (Windows: an Explorer window or the desktop), otherwise Downloads.

use std::path::PathBuf;

use crate::input::Target;

/// `at` is the drop point on the streamed monitor (0..1); `target` is that
/// monitor's place on the desktop.
pub fn folder_for(at: Option<(f32, f32)>, target: Option<Target>) -> PathBuf {
    #[cfg(windows)]
    if let (Some((x, y)), Some(t)) = (at, target) {
        let px = t.mon_x + (x.clamp(0.0, 1.0) * t.mon_w.saturating_sub(1) as f32).round() as i32;
        let py = t.mon_y + (y.clamp(0.0, 1.0) * t.mon_h.saturating_sub(1) as f32).round() as i32;
        if let Some(dir) = explorer::folder_at(px, py) {
            return dir;
        }
    }
    // Linux file managers don't say which folder they show; Downloads it is.
    #[cfg(not(windows))]
    let _ = (at, target);
    ac_remote::files::downloads_dir()
}

/// A `file:` URL's path, as Explorer reports it.
#[cfg_attr(not(windows), allow(dead_code))]
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file:")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut it = rest.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = [it.next()?, it.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    let path = String::from_utf8(bytes).ok()?;
    let path = match path.strip_prefix("///") {
        Some(local) => local.to_string(),                         // file:///C:/Users
        None => format!("\\\\{}", path.trim_start_matches('/')), // file://server/share
    };
    Some(PathBuf::from(path.replace('/', "\\")))
}

#[cfg(windows)]
mod explorer {
    use std::path::PathBuf;

    use windows::core::{w, Interface};
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, IServiceProvider, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Shell::{IShellBrowser, IShellWindows, IWebBrowserApp, ShellWindows, SID_STopLevelBrowser};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetAncestor, GetClassNameW, WindowFromPoint, GA_ROOT};

    /// The folder shown at desktop pixel (`x`, `y`), if that is Explorer or the desktop.
    pub fn folder_at(x: i32, y: i32) -> Option<PathBuf> {
        let root = unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) };
        if root.is_invalid() {
            return None;
        }
        match class(root).as_str() {
            "CabinetWClass" | "ExploreWClass" => explorer_folder(root),
            "Progman" | "WorkerW" => directories::UserDirs::new()?.desktop_dir().map(Into::into),
            _ => None,
        }
    }

    fn class(h: HWND) -> String {
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(h, &mut buf) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    /// The folder in Explorer window `root`. Windows 11's tabs share one
    /// window; the tab showing is the first `ShellTabWindowClass` child.
    fn explorer_folder(root: HWND) -> Option<PathBuf> {
        let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let found = (|| unsafe {
            let shell: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
            let shown_tab = FindWindowExW(Some(root), None, w!("ShellTabWindowClass"), None).ok();
            let mut first = None;
            for i in 0..shell.Count().ok()? {
                let Ok(disp) = shell.Item(&VARIANT::from(i)) else { continue };
                let Ok(app) = disp.cast::<IWebBrowserApp>() else { continue };
                if app.HWND().ok().map(|h| h.0) != Some(root.0 as isize) {
                    continue;
                }
                let Some(path) = app.LocationURL().ok().and_then(|u| super::file_url_path(&u.to_string())) else { continue };
                let tab = disp
                    .cast::<IServiceProvider>()
                    .ok()
                    .and_then(|sp| sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser).ok())
                    .and_then(|b| b.GetWindow().ok());
                if tab.is_some() && tab == shown_tab {
                    return Some(path);
                }
                first.get_or_insert(path);
            }
            first
        })();
        if init.is_ok() {
            unsafe { CoUninitialize() };
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_urls() {
        assert_eq!(file_url_path("file:///C:/Users/me/My%20Files"), Some(PathBuf::from(r"C:\Users\me\My Files")));
        assert_eq!(file_url_path("file://server/share/a"), Some(PathBuf::from(r"\\server\share\a")));
        assert_eq!(file_url_path("file:///C:/caf%C3%A9"), Some(PathBuf::from(r"C:\café")));
        assert_eq!(file_url_path("::{20D04FE0}"), None);
    }
}
