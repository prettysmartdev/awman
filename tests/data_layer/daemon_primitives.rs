//! Unified daemon paths and private on-disk identity.
use awman::data::fs::{
    daemon_process::{DaemonProcess, DAEMON_PLIST_LABEL, DAEMON_UNIT_NAME},
    DataPaths,
};
#[test]
fn daemon_identity_is_under_the_shared_data_root() {
    let paths = DataPaths::at_root("/home/example/data").daemon();
    assert_eq!(
        paths.pid_file(),
        std::path::PathBuf::from("/home/example/data/daemon/awman.pid")
    );
    assert_eq!(paths.startup_lock_file(), paths.root().join("startup.lock"));
    assert_eq!(paths.api_keys_file(), paths.root().join("api_keys.json"));
    assert_eq!(paths.tls_sans_file(), paths.root().join("tls/sans.json"));
    #[cfg(unix)]
    assert_eq!(paths.local_endpoint(), paths.root().join("api.sock"));
}
#[test]
fn pidfile_claim_and_owned_release_are_private_and_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let paths = DataPaths::at_root(dir.path()).daemon();
    let process = DaemonProcess::unified(paths.clone());
    assert_eq!(process.unit_name(), DAEMON_UNIT_NAME);
    assert_eq!(process.plist_label(), DAEMON_PLIST_LABEL);
    assert!(process.claim_pidfile(42).unwrap());
    assert!(!process.claim_pidfile(99).unwrap());
    assert_eq!(process.read_pid().unwrap(), Some(42));
    assert!(!process.release_pidfile_owned_by(99).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(paths.pid_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(paths.root())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert!(process.release_pidfile_owned_by(42).unwrap());
    process.force_write_pidfile(99).unwrap();
    assert_eq!(process.read_pid().unwrap(), Some(99));
}
