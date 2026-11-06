//! Single daemon identity and process supervision regressions.
use awman::data::{
    config::env::EnvSnapshot,
    fs::{
        daemon_process::{DAEMON_PLIST_LABEL, DAEMON_UNIT_NAME},
        DataPaths,
    },
};
use awman::engine::daemon::{AcquireError, DaemonGuard, DaemonSupervisor, Termination};
#[test]
fn legacy_selectors_resolve_the_same_process_identity() {
    let dir = tempfile::tempdir().unwrap();
    let env = EnvSnapshot::with_overrides([("AWMAN_HOME", dir.path().to_string_lossy().as_ref())]);
    let api = DaemonGuard::for_env(&env).unwrap();
    let squad = DaemonGuard::for_env(&env).unwrap();
    assert_eq!(api.paths(), squad.paths());
    assert_eq!(api.paths().root(), dir.path().join("data/daemon"));
    assert_eq!(api.supervisor().process().unit_name(), DAEMON_UNIT_NAME);
    assert_eq!(api.supervisor().process().plist_label(), DAEMON_PLIST_LABEL);
}
#[test]
fn acquire_refuses_live_identity_and_recovers_stale_identity() {
    let daemon = LiveDaemon::spawn();
    let dir = tempfile::tempdir().unwrap();
    let guard = DaemonGuard::new(DataPaths::at_root(dir.path()).daemon());
    guard
        .supervisor()
        .process()
        .force_write_pidfile(i32::MAX as u32)
        .unwrap();
    assert_eq!(guard.running_pid().unwrap(), None);
    guard.acquire(daemon.child.id()).unwrap().finish();
    assert!(matches!(
        guard.acquire(std::process::id()),
        Err(AcquireError::AlreadyRunning { .. })
    ));
    assert!(guard.release_owned_by(daemon.child.id()).unwrap());
}
#[test]
fn stale_termination_cleans_the_pidfile_without_signalling() {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = DaemonSupervisor::for_paths(DataPaths::at_root(dir.path()).daemon());
    supervisor
        .process()
        .force_write_pidfile(i32::MAX as u32)
        .unwrap();
    assert!(matches!(
        supervisor.terminate_running().unwrap(),
        Termination::StalePidFile { .. }
    ));
    assert!(supervisor.process().read_pid().unwrap().is_none());
}
#[test]
fn concurrent_start_has_one_winner_and_holds_lock_until_finish() {
    let daemon = LiveDaemon::spawn();
    let pid = daemon.child.id();
    let dir = tempfile::tempdir().unwrap();
    let paths = DataPaths::at_root(dir.path()).daemon();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let joins: Vec<_> = (0..2)
        .map(|_| {
            let paths = paths.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let guard = DaemonGuard::new(paths);
                barrier.wait();
                match guard.acquire(pid) {
                    Ok(claim) => {
                        assert!(guard.paths().startup_lock_file().exists());
                        claim.finish();
                        true
                    }
                    Err(AcquireError::AlreadyRunning { .. }) => false,
                    Err(e) => panic!("unexpected {e}"),
                }
            })
        })
        .collect();
    assert_eq!(
        joins
            .into_iter()
            .map(|j| j.join().unwrap() as usize)
            .sum::<usize>(),
        1
    );
}

/// The integration test executable is named engine_tests, so use a real
/// awman-named process to exercise PID identity checks without weakening them.
struct LiveDaemon {
    _dir: tempfile::TempDir,
    child: std::process::Child,
}
impl LiveDaemon {
    fn spawn() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join(if cfg!(windows) {
            "awman-guard-helper.exe"
        } else {
            "awman-guard-helper"
        });
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let mut command = std::process::Command::new(executable);
        command
            .args([
                "--exact",
                "daemon_supervisor::live_daemon_test_helper",
                "--ignored",
            ])
            .env("AWMAN_RUNTIME_GUARD_HELPER", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // A concurrent fork can briefly inherit the copy's writable file
        // descriptor until it execs. Retry only refused (never executed) execs.
        let mut retries = 0;
        let child = loop {
            match command.spawn() {
                Ok(child) => break child,
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy && retries < 50 =>
                {
                    retries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("spawn the guard fixture: {error}"),
            }
        };
        assert!(DaemonSupervisor::pid_is_awman(child.id()));
        Self { _dir: dir, child }
    }
}
impl Drop for LiveDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
#[ignore = "child-process fixture; invoked explicitly by LiveDaemon"]
fn live_daemon_test_helper() {
    assert_eq!(
        std::env::var("AWMAN_RUNTIME_GUARD_HELPER").as_deref(),
        Ok("1")
    );
    std::thread::sleep(std::time::Duration::from_secs(60));
}
