//! Part 0 daemon primitive regression tests — the Layer 1 half.
//!
//! Background spawning, per-daemon identity, and `DaemonGuard`'s cross-daemon
//! mutual exclusion. Split out of `tests/data_layer/daemon_primitives.rs` by
//! WI 0114 F-29, which moved these from Layer 0 to `engine::daemon`.
//!
//! These spawn real processes; the Layer 0 half stays hermetic.

use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

use awman::data::config::env::EnvSnapshot;
use awman::data::fs::daemon_process::{
    DaemonProcess, API_PLIST_LABEL, API_UNIT_NAME, SQUAD_PLIST_LABEL, SQUAD_UNIT_NAME,
};
use awman::data::fs::data_paths::AWMAN_HOME;
use awman::data::fs::DaemonPaths;
use awman::engine::daemon::{DaemonGuard, DaemonKind, DaemonSupervisor};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::process_env::ProcessEnvGuard;

/// An env whose `AWMAN_HOME` is the common parent of `api_root` and
/// `squad_root` (which must be `<home>/api` and `<home>/squad`).
fn paths_env(api_root: &Path, squad_root: &Path) -> EnvSnapshot {
    let home = api_root.parent().expect("api root has a parent");
    assert_eq!(api_root, home.join("api"));
    assert_eq!(squad_root, home.join("squad"));
    EnvSnapshot::with_overrides([(AWMAN_HOME, home.to_string_lossy().to_string())])
}

fn daemon(root: &Path, kind: DaemonKind) -> DaemonSupervisor {
    DaemonSupervisor::new(match kind {
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
    })
}

#[test]
fn spawn_detached_identity_is_distinct_for_api_and_squad() {
    let api_root = tempfile::tempdir().unwrap();
    let squad_root = tempfile::tempdir().unwrap();
    // Both launchers are stubbed below, under a scoped `PATH` (and `HOME` on
    // macOS), so the routing is exercised without reaching a real one.
    let api = daemon(api_root.path(), DaemonKind::Api).with_stubbed_service_manager();
    let squad = daemon(squad_root.path(), DaemonKind::Squad).with_stubbed_service_manager();

    assert_ne!(API_UNIT_NAME, SQUAD_UNIT_NAME);
    assert_ne!(API_PLIST_LABEL, SQUAD_PLIST_LABEL);
    assert_ne!(
        api.process().paths().log_file(),
        squad.process().paths().log_file()
    );

    #[cfg(target_os = "linux")]
    {
        let mut env = ProcessEnvGuard::lock();
        let bin_dir = api_root.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let log = api_root.path().join("systemd-run-args");
        let fake_systemd = bin_dir.join("systemd-run");
        std::fs::write(
            &fake_systemd,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\nprintf '%s\\n' \"$*\" >> \"$AWMAN_TEST_SYSTEMD_LOG\"\nexit 0\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_systemd, std::fs::Permissions::from_mode(0o755)).unwrap();
        env.prepend_path(&bin_dir);
        env.set("AWMAN_TEST_SYSTEMD_LOG", &log);
        assert_eq!(api.spawn_detached(Path::new("/bin/true"), &[]).unwrap(), 0);
        assert_eq!(
            squad.spawn_detached(Path::new("/bin/true"), &[]).unwrap(),
            0
        );
        drop(env);
        let args = std::fs::read_to_string(log).unwrap();
        assert!(
            args.contains("--unit=awman-api"),
            "API unit was not threaded: {args}"
        );
        assert!(
            args.contains("--unit=awman-squad"),
            "squad unit was not threaded: {args}"
        );
    }

    // The launchd path is the macOS equivalent: the plist filename *and* its
    // `Label` key must both be the daemon's own, or the second daemon would
    // overwrite the first's launch agent. `try_launchd` resolves the plist
    // under `dirs::home_dir()` and shells out to `launchctl load`, so a
    // scoped `HOME` plus a stub `launchctl` on `PATH` exercises it for real.
    #[cfg(target_os = "macos")]
    {
        // The module-wide lock, not a private one: this block installs a
        // `launchctl` stub that reports *success*, and the spawn-failure test
        // installs one that reports failure. Two locks over one process-global
        // `PATH` serialize nothing, and whichever stub happened to be
        // installed would decide the other test's result.
        let mut env = ProcessEnvGuard::lock();
        let home = tempfile::tempdir().unwrap();
        let bin_dir = home.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let fake_launchctl = bin_dir.join("launchctl");
        std::fs::write(&fake_launchctl, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_launchctl, std::fs::Permissions::from_mode(0o755)).unwrap();

        env.prepend_path(&bin_dir);
        env.set("HOME", home.path());
        api.spawn_detached(Path::new("/usr/bin/true"), &[]).unwrap();
        squad
            .spawn_detached(Path::new("/usr/bin/true"), &[])
            .unwrap();

        drop(env);

        let agents = home.path().join("Library/LaunchAgents");
        let api_plist = std::fs::read_to_string(agents.join(format!("{API_PLIST_LABEL}.plist")))
            .expect("the API daemon must write its own plist");
        let squad_plist =
            std::fs::read_to_string(agents.join(format!("{SQUAD_PLIST_LABEL}.plist")))
                .expect("the squad daemon must write its own plist, not overwrite the API's");
        assert!(api_plist.contains(&format!("<string>{API_PLIST_LABEL}</string>")));
        assert!(squad_plist.contains(&format!("<string>{SQUAD_PLIST_LABEL}</string>")));
        // Each daemon's stdout goes to its own log, never the other's.
        assert!(api_plist.contains(&api.process().paths().log_file().display().to_string()));
        assert!(squad_plist.contains(&squad.process().paths().log_file().display().to_string()));
        assert!(!api_plist.contains(&squad.process().paths().log_file().display().to_string()));
    }
}

#[test]
fn squad_spawn_failure_is_wrapped_with_an_attributable_daemon_message() {
    let tmp = tempfile::tempdir().unwrap();
    let process = daemon(tmp.path(), DaemonKind::Squad);

    // Force the portable `double_fork_spawn` path.  Both platform launchers
    // *accept* a job before its executable fails — a real systemd-run accepts
    // the unit, and `launchctl load` accepts a plist naming a binary that does
    // not exist — which is correct production behavior but not a synchronous
    // failure injection.  Stubbing the launcher so it reports itself
    // unavailable is what makes the missing binary observable here.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let _path_guard = {
        let mut env = ProcessEnvGuard::lock();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let launcher = bin_dir.join(if cfg!(target_os = "macos") {
            "launchctl"
        } else {
            "systemd-run"
        });
        std::fs::write(&launcher, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Prepend rather than replace: the stub only needs to shadow the real
        // launcher. Other tests in this binary spawn `git` concurrently and
        // must keep finding it on the process-global `PATH`.
        env.prepend_path(&bin_dir);
        env
    };

    let error = process
        .spawn_detached(Path::new("/definitely-not-an-awman-daemon-binary"), &[])
        .expect_err("a missing daemon binary must surface a spawn failure");
    let text = error.to_string();
    assert!(
        text.starts_with("failed to start the squad daemon:"),
        "daemon startup errors must be attributable instead of raw OS diagnostics: {text}"
    );
    assert!(
        !text.starts_with("Load failed:"),
        "the raw launchd diagnostic must never be surfaced verbatim: {text}"
    );
}

#[test]
fn daemon_guard_check_accepts_absent_and_stale_pidfiles_in_both_directions() {
    let tmp = tempfile::tempdir().unwrap();
    let api_root = tmp.path().join("api");
    let squad_root = tmp.path().join("squad");
    let env = paths_env(&api_root, &squad_root);

    for kind in [DaemonKind::Api, DaemonKind::Squad] {
        let guard = DaemonGuard::for_daemon(kind, &env).unwrap();
        assert!(
            guard.check().is_ok(),
            "absent pidfile must be allowed: {kind:?}"
        );

        let other_kind = match kind {
            DaemonKind::Api => DaemonKind::Squad,
            DaemonKind::Squad => DaemonKind::Api,
        };
        let other_root = match other_kind {
            DaemonKind::Api => &api_root,
            DaemonKind::Squad => &squad_root,
        };
        daemon(other_root, other_kind)
            .process()
            .force_write_pidfile(u32::MAX - 1)
            .unwrap();
        assert!(
            guard.check().is_ok(),
            "stale pidfile must be allowed: {kind:?}"
        );
        assert!(!daemon(other_root, other_kind)
            .process()
            .paths()
            .pid_file()
            .exists());
    }
}

#[cfg(unix)]
fn awman_named_test_process() -> (Child, tempfile::TempDir) {
    // The guard deliberately rejects a live PID belonging to an unrelated
    // process. Copying the test harness to an `awman` basename gives the
    // child the same process identity a real daemon has without starting a
    // server or opening a database.
    let source = std::env::current_exe().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let helper = tmp.path().join("awman");
    let ready = tmp.path().join("ready");
    std::fs::copy(source, &helper).unwrap();
    // A concurrent test function forking anywhere in this process can inherit
    // the still-open write descriptor on `helper`, which makes `execve` report
    // the file as busy (`ETXTBSY`) until that fork execs and drops it. The
    // window is short, so retry rather than force `--test-threads=1`.
    let mut attempt = 0;
    let mut child = loop {
        let spawned = Command::new(&helper)
            // libtest names integration tests by module path, so `--exact` must
            // carry this file's `daemon_supervisor::` prefix (its mount in
            // `tests/engine/main.rs`). A name that matches nothing makes the
            // helper run zero tests and exit at once, leaving a zombie whose
            // command name macOS `ps` no longer reports as awman.
            .args([
                "--exact",
                "daemon_supervisor::daemon_guard_helper_process_stays_alive",
                "--ignored",
                "--nocapture",
            ])
            .env("AWMAN_GUARD_HELPER", &ready)
            .spawn();
        match spawned {
            Ok(child) => break child,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("spawn awman-named helper process: {e}"),
        }
    };
    // Wait for the ignored test body to start, as well as the process name.
    // The executable can appear in `ps` before libtest has selected the test
    // body; an unmatched filter would then exit before the guard checks it.
    for _ in 0..500 {
        if ready.exists() && DaemonSupervisor::pid_is_awman(child.id()) {
            // Keep the copied executable until the caller stops the child.
            // macOS uses its path to report the command name to `ps`.
            return (child, tmp);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("awman-named helper process never entered its test body with an awman command name");
}

#[cfg(unix)]
fn assert_guard_rejects_live_other(kind: DaemonKind) {
    let tmp = tempfile::tempdir().unwrap();
    let api_root = tmp.path().join("api");
    let squad_root = tmp.path().join("squad");
    let env = paths_env(&api_root, &squad_root);
    let other_kind = match kind {
        DaemonKind::Api => DaemonKind::Squad,
        DaemonKind::Squad => DaemonKind::Api,
    };
    let other_root = match other_kind {
        DaemonKind::Api => &api_root,
        DaemonKind::Squad => &squad_root,
    };
    let (mut child, _helper_dir) = awman_named_test_process();
    for _ in 0..40 {
        if DaemonSupervisor::is_process_alive(child.id()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(DaemonSupervisor::is_process_alive(child.id()));
    daemon(other_root, other_kind)
        .process()
        .force_write_pidfile(child.id())
        .unwrap();

    let error = DaemonGuard::for_daemon(kind, &env)
        .unwrap()
        .check()
        .expect_err("a live other daemon must block startup");
    let text = error.to_string();
    assert!(
        text.contains(&child.id().to_string()),
        "missing PID: {text}"
    );
    match other_kind {
        DaemonKind::Api => assert!(text.contains("awman api"), "missing API name: {text}"),
        DaemonKind::Squad => assert!(text.contains("squad"), "missing squad name: {text}"),
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn daemon_guard_check_rejects_live_squad_when_checking_api() {
    assert_guard_rejects_live_other(DaemonKind::Api);
}

#[cfg(unix)]
#[test]
fn daemon_guard_check_rejects_live_api_when_checking_squad() {
    assert_guard_rejects_live_other(DaemonKind::Squad);
}

/// Not a test: the body the guard helper process runs. Ignored, so it runs
/// only when the helper asks for it by name.
#[test]
#[ignore = "runs only as the body of the daemon-guard helper process"]
fn daemon_guard_helper_process_stays_alive() {
    if let Some(ready) = std::env::var_os("AWMAN_GUARD_HELPER") {
        std::fs::write(ready, b"ready").unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }
}
