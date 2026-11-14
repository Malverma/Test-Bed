//! Remote access to PCs on the local network, ported from RemoteLinux:
//! protocol, transport, identity, pairing, config, and the viewer's
//! connection. The wire protocol, port and mDNS name are RemoteLinux's, so
//! Test Bed and RemoteLinux interoperate.

pub mod client;
pub mod clock;
pub mod config;
pub mod files;
pub mod connection;
pub mod discovery;
pub mod hostctl;
pub mod identity;
pub mod internet;
pub mod ipc;
pub mod net;
pub mod pairing;
#[cfg(not(target_os = "android"))]
pub mod playback;
pub mod paths;
pub mod proto;
pub mod resample;
pub mod tls;
pub mod transport;
pub mod trust;
pub mod video;

/// Wire protocol version. Bumped on any incompatible change.
pub const PROTOCOL_VERSION: u32 = 1;
/// Default UDP port the host listens on.
pub const DEFAULT_PORT: u16 = 47800;

/// This machine's hostname, used as the default device name.
pub fn hostname() -> String {
    #[cfg(windows)]
    if let Ok(n) = std::env::var("COMPUTERNAME") {
        return n;
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
