use crate::support;
use awman_test_support::capability::PreviousRelease;
#[test]
#[cfg(target_os = "macos")]
fn previous_release_state_upgrades_and_boots() {
    published_upgrade("upgrade::previous_release_state_upgrades_and_boots");
}

/// Selected only when the manifest pins a published Linux builtin predecessor.
#[test]
#[cfg(target_os = "linux")]
fn linux_published_previous_state_upgrades_and_boots() {
    published_upgrade("upgrade::linux_published_previous_state_upgrades_and_boots");
}

fn published_upgrade(id: &'static str) {
    let mut s = support::sandbox(id);
    // v0.13 reads AWMAN_CONFIG_HOME; current awman reads AWMAN_HOME. Both
    // binaries must use the same private configuration and short stateDir.
    let awman_home = s.awman_home();
    s.env_mut().set("AWMAN_CONFIG_HOME", &awman_home);
    let archive = support::fixture(&s);
    let previous = s.require::<PreviousRelease>();
    s.grant(&previous);
    s.note(
        "previous_release_sha256",
        &awman_test_support::fixtures::sha256_file(&previous.exe).unwrap(),
    );
    support::watch(&s);
    let local = s.work().join("source.tar");
    std::fs::copy(&archive, &local).unwrap();
    let repo = support::session::prepare(
        &s,
        &local,
        "claude",
        "#!/bin/sh\nIFS= read -r prompt\necho UPGRADE_GUEST_UID=$(id -u)\necho $prompt\n",
    );
    let saved = support::session::previous_schema(&s, &previous.exe, &repo, &local);
    support::session::ready(&s, &previous.exe, &repo);
    let old = support::session::exec(&s, &previous.exe, &repo, "claude", "before-upgrade");
    old.assert_success();
    assert!(
        old.stdout_text().contains("UPGRADE_GUEST_UID=1234"),
        "{old:?}"
    );
    let marker = support::state(&s).join("awman-runtime-version");
    // The on-disk marker is checked by the current binary's upgrade transaction.
    let before =
        std::fs::read_to_string(&marker).expect("previous published binary must create state");
    s.note("previous_protocol", &before);
    std::fs::remove_file(&local).unwrap(); // new executable must use cached images
    support::session::restore_schema(&s, &repo, &local, saved);
    support::session::ready(&s, support::awman_exe(), &repo);
    let new = support::session::exec(&s, support::awman_exe(), &repo, "claude", "after-upgrade");
    new.assert_success();
    assert!(
        new.stdout_text().contains("UPGRADE_GUEST_UID=1234"),
        "{new:?}"
    );
    assert!(new.stdout_text().contains("after-upgrade"));
    assert_eq!(
        std::fs::read_to_string(marker).unwrap().trim(),
        awman::data::builtin_runtime::WORKER_PROTOCOL
    );
}

/// Linux has no published builtin predecessor yet. Prove the install/update
/// boundary while an actual guest is running from the replaced inode, then
/// boot from cached state with the new executable. This is not a historical
/// protocol migration.
#[test]
#[cfg(target_os = "linux")]
fn cached_state_survives_atomic_executable_replacement() {
    let mut s = support::sandbox("upgrade::cached_state_survives_atomic_executable_replacement");
    let archive = support::fixture(&s);
    support::watch(&s);
    let local = s.work().join("source.tar");
    std::fs::copy(archive, &local).unwrap();
    let repo = support::session::prepare(
        &s,
        &local,
        "claude",
        "#!/bin/sh\nset -eu\nIFS= read -r prompt\necho REPLACEMENT_GUEST_UID=$(id -u)\nif [ \"$prompt\" = before-replace ]; then echo ready > /replacement/ready; while [ ! -f /replacement/release ]; do sleep 0.05; done; fi\necho $prompt\n",
    );
    let barriers = repo.join(".replacement");
    std::fs::create_dir(&barriers).unwrap();
    let config_path = repo.join(".awman/config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["overlays"]
        .as_array_mut()
        .unwrap()
        .push(format!("dir({}:/replacement:rw)", barriers.display()).into());
    std::fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let installed = s.root().join("installed-awman");
    std::fs::copy(support::awman_exe(), &installed).unwrap();
    s.register_exe("installed-awman", &installed);
    support::session::ready(&s, &installed, &repo);
    let mut running = s
        .command(&installed)
        .current_dir(&repo)
        .env("AWMAN_TEST_BUILTIN", "1")
        .args([
            "exec",
            "prompt",
            "-n",
            "--agent",
            "claude",
            "before-replace",
        ])
        .ready(awman_test_support::Ready::FileExists(
            barriers.join("ready"),
        ))
        .spawn();
    let replacement = s.root().join("replacement-awman");
    std::fs::copy(support::awman_exe(), &replacement).unwrap();
    std::fs::rename(replacement, &installed).unwrap();
    let running_executable = std::fs::read_link(format!("/proc/{}/exe", running.pid())).unwrap();
    assert!(
        running_executable.to_string_lossy().ends_with(" (deleted)"),
        "the running CLI still has its original inode on disk: {}",
        running_executable.display()
    );
    std::fs::write(barriers.join("release"), b"continue").unwrap();
    assert_eq!(
        running.wait(std::time::Duration::from_secs(60)),
        awman_test_support::Exit::Code(0),
        "{}",
        running.stderr_so_far()
    );
    let completed = running.stdout_so_far();
    assert!(
        completed.contains("REPLACEMENT_GUEST_UID=1234"),
        "{completed}"
    );
    assert!(completed.contains("before-replace"), "{completed}");
    std::fs::remove_file(local).unwrap();
    let after = support::session::exec(&s, &installed, &repo, "claude", "after-replace");
    after.assert_success();
    assert!(after.stdout_text().contains("REPLACEMENT_GUEST_UID=1234"));
    assert!(after.stdout_text().contains("after-replace"));
    assert_eq!(
        std::fs::read_to_string(support::state(&s).join("awman-runtime-version"))
            .unwrap()
            .trim(),
        awman::data::builtin_runtime::WORKER_PROTOCOL,
    );
}
