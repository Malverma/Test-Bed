//! Bringing over RemoteLinux's pairings. Its own process: it sets the XDG
//! variables.

use std::fs;

#[test]
fn copies_remotelinux_once_and_never_over_test_beds_own() {
    let root = std::env::temp_dir().join(format!("ac-remote-migrate-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (cfg, data) = (root.join("config"), root.join("data"));
    fs::create_dir_all(cfg.join("remotelinux")).unwrap();
    fs::create_dir_all(data.join("remotelinux/identity/host")).unwrap();
    fs::write(cfg.join("remotelinux/authorized_clients.toml"), "[[client]]\nname = \"Laptop\"\nfingerprint = \"ab\"\npaired_at = 1\n").unwrap();
    fs::write(data.join("remotelinux/identity/host/cert.der"), b"cert").unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &cfg);
    std::env::set_var("XDG_DATA_HOME", &data);

    assert!(ac_remote::paths::migrate_from_remotelinux());
    assert_eq!(ac_remote::trust::AuthorizedClients::load().unwrap().clients[0].name, "Laptop");
    assert_eq!(fs::read(data.join("test-bed/remote/identity/host/cert.der")).unwrap(), b"cert");

    // Test Bed's own files now exist: a second run leaves them alone.
    fs::write(cfg.join("remotelinux/authorized_clients.toml"), "").unwrap();
    assert!(!ac_remote::paths::migrate_from_remotelinux());
    assert_eq!(ac_remote::trust::AuthorizedClients::load().unwrap().clients.len(), 1);
    let _ = fs::remove_dir_all(&root);
}
