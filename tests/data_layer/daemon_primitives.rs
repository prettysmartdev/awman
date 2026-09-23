//! Part 0 daemon primitive regression tests — the Layer 0 half.
//!
//! Paths, PID files and the `ServerMeta` sidecar: everything
//! `data::fs::daemon_process` still owns after WI 0114 F-29 moved process
//! spawning, liveness probing and the daemon guard up to Layer 1. The engine
//! half lives in `tests/engine/daemon_supervisor.rs`.

use std::path::{Path, PathBuf};

use awman::data::fs::daemon_process::{
    DaemonProcess, ServerMeta, API_PLIST_LABEL, API_UNIT_NAME, SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME,
};
use awman::data::fs::{DaemonPaths, SquadPaths};
use awman::engine::daemon::DaemonKind;

fn daemon(root: &Path, kind: DaemonKind) -> DaemonProcess {
    match kind {
        DaemonKind::Api => DaemonProcess::new(
            DaemonPaths::new(root, "api_key"),
            API_UNIT_NAME,
            API_PLIST_LABEL,
        ),
        DaemonKind::Squad => DaemonProcess::new(
            DaemonPaths::new(root, "squad_key"),
            SQUAD_UNIT_NAME,
            SQUAD_PLIST_LABEL,
        ),
    }
}

#[test]
fn daemon_paths_preserve_api_filenames_and_isolate_squad_key() {
    let api = DaemonPaths::new("/tmp/api", "api_key");
    assert_eq!(api.pid_file(), PathBuf::from("/tmp/api/awman.pid"));
    assert_eq!(api.log_file(), PathBuf::from("/tmp/api/awman.log"));
    assert_eq!(
        api.server_meta_file(),
        PathBuf::from("/tmp/api/server.json")
    );
    assert_eq!(api.key_hash_file(), PathBuf::from("/tmp/api/api_key.hash"));

    let squad = SquadPaths::from_root("/tmp/squad").daemon();
    assert_eq!(squad.pid_file(), PathBuf::from("/tmp/squad/awman.pid"));
    assert_eq!(squad.log_file(), PathBuf::from("/tmp/squad/awman.log"));
    assert_eq!(
        squad.server_meta_file(),
        PathBuf::from("/tmp/squad/server.json")
    );
    assert_eq!(
        squad.key_hash_file(),
        PathBuf::from("/tmp/squad/squad_key.hash")
    );
    assert_ne!(api.key_hash_file(), squad.key_hash_file());
}

#[test]
fn daemon_process_pidfile_and_server_meta_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let process = daemon(tmp.path(), DaemonKind::Api);

    assert_eq!(process.read_pid().unwrap(), None);
    assert!(process.claim_pidfile(4242).unwrap());
    assert!(!process.claim_pidfile(9999).unwrap());
    assert_eq!(process.read_pid().unwrap(), Some(4242));
    process.release_pidfile().unwrap();
    assert_eq!(process.read_pid().unwrap(), None);

    let meta = ServerMeta {
        port: 3210,
        bind_ip: "127.0.0.1".into(),
        scheme: "http".into(),
        auth_disabled: false,
    };
    assert_eq!(process.read_meta().unwrap(), None);
    process.write_meta(&meta).unwrap();
    assert_eq!(process.read_meta().unwrap(), Some(meta));
    process.clear_meta().unwrap();
    assert_eq!(process.read_meta().unwrap(), None);
}
