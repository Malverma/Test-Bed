//! Starting the share service on Windows: a `Run` entry under the user's
//! registry key starts it at sign-in, in the signed-in session (it needs the
//! desktop to capture it). Nothing needs administrator rights.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Test Bed Share";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// Runs a console tool without flashing a window.
fn quiet(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program).args(args).creation_flags(CREATE_NO_WINDOW).output().with_context(|| format!("running {program}"))
}

/// Whether sharing starts at sign-in.
pub fn service_enabled() -> bool {
    quiet("reg", &["query", RUN_KEY, "/v", RUN_VALUE]).map(|o| o.status.success()).unwrap_or(false)
}

/// The binary the Run entry starts.
pub fn service_binary() -> Option<PathBuf> {
    let o = quiet("reg", &["query", RUN_KEY, "/v", RUN_VALUE]).ok()?;
    let out = String::from_utf8_lossy(&o.stdout);
    let line = out.lines().find(|l| l.contains("REG_SZ"))?;
    let value = line.split("REG_SZ").nth(1)?.trim();
    value.strip_prefix('"')?.split('"').next().map(PathBuf::from)
}

/// Start `bin` now and at every sign-in.
pub fn enable_service(bin: &Path) -> Result<()> {
    let command = format!("\"{}\" run", bin.display());
    let o = quiet("reg", &["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &command, "/f"])?;
    if !o.status.success() {
        bail!("couldn't add the sign-in entry: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    stop();
    start(bin)
}

pub fn restart_service() -> Result<()> {
    let bin = service_binary().or_else(super::host_binary).context("test-bed-share.exe not found")?;
    stop();
    start(&bin)
}

/// Stop sharing and keep it from starting at sign-in.
pub fn disable_service() -> Result<()> {
    let _ = quiet("reg", &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
    stop();
    Ok(())
}

fn start(bin: &Path) -> Result<()> {
    Command::new(bin)
        .arg("run")
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", bin.display()))?;
    Ok(())
}

/// Ends this user's share service, and waits until its pipe is free.
fn stop() {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let _ = quiet("taskkill", &["/F", "/IM", "test-bed-share.exe", "/FI", &format!("USERNAME eq {user}")]);
    for _ in 0..20 {
        if !Path::new(&crate::paths::host_control_socket()).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// RemoteLinux never ran on Windows.
pub fn remotelinux_enabled() -> bool {
    false
}

pub fn retire_remotelinux() -> Result<()> {
    Ok(())
}

/// The last lines of the service's log file.
pub fn service_log(lines: usize) -> String {
    let text = std::fs::read_to_string(crate::paths::share_log()).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

// Linux's one-time setup has no Windows counterpart: SendInput needs no
// permission, and Windows asks about the firewall itself the first time the
// service listens.

pub fn uinput_writable() -> bool {
    true
}

pub fn input_readable() -> bool {
    true
}

pub fn ufw_active() -> bool {
    false
}

pub fn firewall_opened() -> bool {
    true
}

pub fn open_firewall() -> Result<()> {
    Ok(())
}

pub fn fix_uinput() -> Result<()> {
    Ok(())
}
