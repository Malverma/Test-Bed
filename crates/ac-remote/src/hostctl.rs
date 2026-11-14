//! Managing the local host daemon from a frontend: talking to its control
//! socket, starting it at login (a systemd user service on Linux, a Run
//! entry on Windows), and Linux's one-time system setup (firewall, uinput
//! access) through pkexec.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};

use crate::ipc::{Reader, Writer};

pub const SERVICE: &str = "test-bed-share.service";
/// The standalone RemoteLinux host, which Test Bed replaces (same port).
pub const REMOTELINUX_SERVICE: &str = "rl-host.service";
#[cfg(unix)]
const BINARY: &str = "test-bed-share";
#[cfg(windows)]
const BINARY: &str = "test-bed-share.exe";

/// Status reported by a running host daemon.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HostStatus {
    pub name: String,
    pub listen: String,
    pub port: u16,
    pub fingerprint: String,
    pub pairing_open: bool,
    pub client: Option<ConnectedClient>,
    #[serde(default)]
    pub paired: Vec<PairedClient>,
    #[serde(default)]
    pub internet: InternetStatus,
}

/// The daemon's state on the internet (spec3).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct InternetStatus {
    pub enabled: bool,
    pub id: Option<String>,
    /// Connected to a relay: other PCs can reach this one.
    pub online: bool,
    pub relay: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectedClient {
    pub name: String,
    pub address: String,
    pub connected_secs: u64,
    pub monitor: Option<String>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub rtt_ms: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PairedClient {
    pub name: String,
    pub fingerprint: String,
    pub fingerprint_short: String,
    pub paired_at: u64,
    /// Paired over the internet.
    #[serde(default)]
    pub internet: bool,
}

async fn connect() -> Result<(Lines<BufReader<Reader>>, Writer)> {
    let (r, w) = crate::ipc::connect().await.context("sharing is not running")?;
    Ok((BufReader::new(r).lines(), w))
}

/// Send one request to the running daemon and return its JSON reply.
pub async fn request(req: Value) -> Result<Value> {
    let (mut lines, mut w) = connect().await?;
    w.write_all(format!("{req}\n").as_bytes()).await?;
    let line = lines.next_line().await?.ok_or_else(|| anyhow!("host closed the control connection"))?;
    Ok(serde_json::from_str(&line)?)
}

/// `None` if the daemon is not running.
pub async fn status() -> Option<HostStatus> {
    let v = request(json!({"cmd": "status"})).await.ok()?;
    serde_json::from_value(v).ok()
}

pub async fn unpair(name: &str) -> Result<bool> {
    let v = request(json!({"cmd": "unpair", "name": name})).await?;
    Ok(v["removed"].as_bool().unwrap_or(false))
}

pub async fn disconnect_client() -> Result<()> {
    request(json!({"cmd": "disconnect"})).await.map(|_| ())
}

/// An open pairing window. Dropping it closes the window on the host.
pub struct PairWindow {
    pub pin: String,
    /// For pairing over the internet: the whole code to send.
    pub invite: Option<String>,
    pub expires_in: u64,
    lines: Lines<BufReader<Reader>>,
    _w: Writer,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PairEvent {
    Paired { name: String },
    WrongPin { attempts_left: u32 },
    Expired,
    Cancelled,
}

impl PairWindow {
    pub async fn open() -> Result<Self> {
        Self::open_cmd("pair").await
    }

    /// Pairing over the internet: an invite code instead of a LAN PIN.
    pub async fn open_invite() -> Result<Self> {
        Self::open_cmd("invite").await
    }

    async fn open_cmd(cmd: &str) -> Result<Self> {
        let (mut lines, mut w) = connect().await?;
        w.write_all(format!("{}\n", json!({ "cmd": cmd })).as_bytes()).await?;
        let line = lines.next_line().await?.ok_or_else(|| anyhow!("host closed the control connection"))?;
        let v: Value = serde_json::from_str(&line)?;
        if let Some(e) = v["error"].as_str() {
            bail!("{e}");
        }
        let pin = v["pin"].as_str().ok_or_else(|| anyhow!("host did not return a PIN: {v}"))?.to_string();
        let invite = v["invite"].as_str().map(str::to_string);
        Ok(Self { pin, invite, expires_in: v["expires_in"].as_u64().unwrap_or(120), lines, _w: w })
    }

    /// Wait for the next pairing event. Returns `Expired` if the host goes away.
    pub async fn next(&mut self) -> PairEvent {
        loop {
            let Ok(Some(line)) = self.lines.next_line().await else { return PairEvent::Expired };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            return match v["event"].as_str() {
                Some("paired") => PairEvent::Paired { name: v["name"].as_str().unwrap_or("").to_string() },
                Some("wrong_pin") => PairEvent::WrongPin { attempts_left: v["attempts_left"].as_u64().unwrap_or(0) as u32 },
                Some("cancelled") => PairEvent::Cancelled,
                Some("expired") => PairEvent::Expired,
                _ => continue,
            };
        }
    }
}

// ---- Service management -------------------------------------------------

/// Turn access from other networks on or off, and restart sharing (if on)
/// so it takes effect.
pub fn set_internet_access(on: bool) -> Result<()> {
    let mut cfg = crate::config::HostConfig::load()?;
    if cfg.internet != on {
        cfg.internet = on;
        cfg.save()?;
    }
    if service_enabled() {
        restart_service()?;
    }
    Ok(())
}

/// The `test-bed-share` binary: next to the current executable, else on PATH.
pub fn host_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join(BINARY))).filter(|p| p.exists()) {
        return Some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(BINARY)).find(|p| p.exists())
}

pub use platform::*;

#[cfg(unix)]
mod platform {
use super::*;
use std::path::Path;
use std::process::Command;

fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("systemctl").arg("--user").args(args).output().context("running systemctl")
}

/// Whether the service is enabled to start with the desktop session.
pub fn service_enabled() -> bool {
    systemctl(&["is-enabled", SERVICE]).map(|o| o.status.success()).unwrap_or(false)
}

/// The user service's unit file for `bin`.
pub(super) fn unit_file(bin: &Path) -> String {
    format!(
        "[Unit]\nDescription=Test Bed: share this PC with paired computers\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n\
         [Service]\nExecStart=\"{}\" run\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=graphical-session.target\n",
        // Quoted: the path may have spaces. `%` starts a systemd specifier.
        bin.display().to_string().replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%")
    )
}

/// The binary an `ExecStart=` line runs.
pub(super) fn exec_binary(unit: &str) -> Option<PathBuf> {
    let exec = unit.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
    let path = match exec.strip_prefix('"') {
        Some(q) => q.split('"').next()?.replace("%%", "%"),
        None => exec.split_whitespace().next()?.to_string(),
    };
    Some(PathBuf::from(path))
}

pub fn restart_service() -> Result<()> {
    let o = systemctl(&["restart", SERVICE])?;
    if !o.status.success() {
        bail!("couldn't restart sharing: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// Install (or update) the user service for `bin` and start it now and at every login.
pub fn enable_service(bin: &Path) -> Result<()> {
    let dir = crate::paths::systemd_user_dir();
    let unit = unit_file(bin);
    crate::paths::write_private(&dir.join(SERVICE), unit.as_bytes(), 0o644)?;
    for args in [&["daemon-reload"][..], &["enable", SERVICE], &["restart", SERVICE]] {
        let o = systemctl(args)?;
        if !o.status.success() {
            bail!("systemctl --user {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
        }
    }
    Ok(())
}

/// Whether the standalone RemoteLinux host is set to start at login.
pub fn remotelinux_enabled() -> bool {
    systemctl(&["is-enabled", REMOTELINUX_SERVICE]).map(|o| o.status.success()).unwrap_or(false)
}

/// Stops the standalone RemoteLinux host for good, so it doesn't hold the port.
pub fn retire_remotelinux() -> Result<()> {
    let o = systemctl(&["disable", "--now", REMOTELINUX_SERVICE])?;
    if !o.status.success() {
        bail!("{}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// The binary the installed service runs, from its unit file.
pub fn service_binary() -> Option<PathBuf> {
    exec_binary(&std::fs::read_to_string(crate::paths::systemd_user_dir().join(SERVICE)).ok()?)
}

/// Stop the service and keep it from starting at login.
pub fn disable_service() -> Result<()> {
    let o = systemctl(&["disable", "--now", SERVICE])?;
    if !o.status.success() {
        bail!("{}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// Recent service log lines, for showing why the host failed to start.
pub fn service_log(lines: usize) -> String {
    Command::new("journalctl")
        .args(["--user", "-u", SERVICE, "-n", &lines.to_string(), "--no-pager", "-o", "cat"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

// ---- One-time system setup (needs root via pkexec) -------------------------

pub fn uinput_writable() -> bool {
    std::fs::OpenOptions::new().write(true).open("/dev/uinput").is_ok()
}

/// Whether this PC's own keyboards and mice can be read, so someone sitting
/// at it can take over from a remote PC (`ac-remote-host`'s `local_input`).
pub fn input_readable() -> bool {
    let Ok(dir) = std::fs::read_dir("/dev/input") else { return true };
    let mut any = false;
    for e in dir.flatten() {
        if !e.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        any = true;
        if std::fs::File::open(e.path()).is_ok() {
            return true;
        }
    }
    !any
}

pub fn ufw_active() -> bool {
    Command::new("systemctl").args(["is-active", "-q", "ufw"]).status().map(|s| s.success()).unwrap_or(false)
}

fn marker() -> PathBuf {
    crate::paths::data_dir().join("firewall-opened")
}

/// Whether we already added the firewall rules (ufw status itself needs root).
pub fn firewall_opened() -> bool {
    marker().exists()
}

/// uinput lets remote PCs type and click; reading the physical keyboards,
/// mice and touchpads lets the person at this PC take over from them.
const UDEV_RULES: [&str; 5] = [
    r#"KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput""#,
    r#"SUBSYSTEM=="input", KERNEL=="event*", ENV{ID_INPUT_KEYBOARD}=="1", TAG+="uaccess""#,
    r#"SUBSYSTEM=="input", KERNEL=="event*", ENV{ID_INPUT_MOUSE}=="1", TAG+="uaccess""#,
    r#"SUBSYSTEM=="input", KERNEL=="event*", ENV{ID_INPUT_TOUCHPAD}=="1", TAG+="uaccess""#,
    r#"SUBSYSTEM=="input", KERNEL=="event*", ENV{ID_INPUT_TOUCHSCREEN}=="1", TAG+="uaccess""#,
];

fn pkexec(script: &str) -> Result<()> {
    let o = Command::new("pkexec").args(["sh", "-c", script]).output().context("running pkexec (is polkit installed?)")?;
    match o.status.code() {
        Some(0) => Ok(()),
        Some(126) | Some(127) => bail!("authorisation was cancelled"),
        _ => bail!("{}", String::from_utf8_lossy(&o.stderr).trim()),
    }
}

/// Allow the remote access UDP port from private LAN ranges in ufw. (Discovery
/// uses mDNS, which ufw allows by default.)
pub fn open_firewall() -> Result<()> {
    let mut script = String::from("set -e;");
    for net in ["192.168.0.0/16", "10.0.0.0/8", "172.16.0.0/12"] {
        script.push_str(&format!("ufw allow proto udp from {net} to any port {} comment Test-Bed >/dev/null;", crate::DEFAULT_PORT));
    }
    pkexec(&script)?;
    crate::paths::write_private(&marker(), b"1", 0o644)
}

/// Install the udev rules that let the logged-in user create virtual input
/// devices and see when the PC's own input is used.
pub fn fix_uinput() -> Result<()> {
    let rules: String = UDEV_RULES.iter().map(|r| format!(" '{r}'")).collect();
    pkexec(&format!(
        "set -e; printf '%s\\n'{rules} > /etc/udev/rules.d/70-test-bed-remote.rules; udevadm control --reload; modprobe uinput || true; udevadm trigger --name-match=uinput; udevadm trigger --subsystem-match=input"
    ))
}

}

#[cfg(windows)]
#[path = "hostctl_windows.rs"]
mod platform;

#[cfg(all(test, unix))]
mod tests {
    use super::platform::*;
    use std::path::Path;

    #[test]
    fn the_unit_runs_a_path_with_spaces() {
        let bin = Path::new("/home/me/Projects/Android Controller/target/debug/test-bed-share");
        let unit = unit_file(bin);
        assert!(unit.contains("ExecStart=\"/home/me/Projects/Android Controller/target/debug/test-bed-share\" run"), "{unit}");
        assert_eq!(exec_binary(&unit).as_deref(), Some(bin));
        assert_eq!(exec_binary("ExecStart=/usr/bin/rl-host run").as_deref(), Some(Path::new("/usr/bin/rl-host")));
    }
}
