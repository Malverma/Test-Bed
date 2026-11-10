use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn xdg_config() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"))
}

fn xdg_data() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share"))
}

/// `~/.config/systemd/user`
pub fn systemd_user_dir() -> PathBuf {
    xdg_config().join("systemd/user")
}

/// `~/.config/test-bed/remote`
pub fn config_dir() -> PathBuf {
    xdg_config().join("test-bed/remote")
}

/// `~/.local/share/test-bed/remote`
pub fn data_dir() -> PathBuf {
    xdg_data().join("test-bed/remote")
}

/// `$XDG_RUNTIME_DIR/test-bed`
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/test-bed-{}", rustix_uid())))
        .join("test-bed")
}

/// Brings over the standalone RemoteLinux's identities, trust stores and
/// settings, once: only when Test Bed has none of its own yet. Keeping the
/// identities means computers paired with RemoteLinux still trust this one.
/// Returns whether anything was copied.
pub fn migrate_from_remotelinux() -> bool {
    let (old_cfg, old_data) = (xdg_config().join("remotelinux"), xdg_data().join("remotelinux"));
    if config_dir().exists() || data_dir().exists() || !(old_cfg.exists() || old_data.exists()) {
        return false;
    }
    let mut copied = false;
    for (from, to) in [(old_cfg, config_dir()), (old_data, data_dir())] {
        match copy_tree(&from, &to) {
            Ok(n) => copied |= n > 0,
            Err(e) => tracing::warn!("bringing over {}: {e}", from.display()),
        }
    }
    if copied {
        tracing::info!("brought over RemoteLinux's pairings and settings");
    }
    copied
}

/// Copies files under `from` to `to`, keeping their permissions.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<usize> {
    if !from.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(to)?;
    let mut n = 0;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            n += copy_tree(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target)?;
            n += 1;
        }
    }
    Ok(n)
}

fn rustix_uid() -> String {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid().to_string()).unwrap_or_else(|_| "user".into())
}

/// Unix socket the host daemon listens on for local control (CLI / UI).
pub fn host_control_socket() -> PathBuf {
    runtime_dir().join("share.sock")
}

/// Write a file atomically with the given permissions.
pub fn write_private(path: &std::path::Path, data: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
