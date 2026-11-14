//! Sharing this PC (ported from RemoteLinux's host): screen capture, encoding, streaming and input injection.

#[cfg(unix)]
pub mod audio;
#[cfg(windows)]
#[path = "audio_windows.rs"]
pub mod audio;
#[cfg(unix)]
pub mod capture;
#[cfg(windows)]
#[path = "capture_windows.rs"]
pub mod capture;
pub mod clipboard;
pub mod control;
pub mod drop_folder;
pub mod encoder;
pub mod input;
pub mod local_input;
pub mod monitors;
pub mod server;
pub mod stream;
