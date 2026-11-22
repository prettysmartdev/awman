//! Exact shipped builtin sessions: signed on Apple Silicon, KVM on Linux.
use crate::support;
#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn signed_artifact_runs_an_offline_session() {
    use awman_test_support::capability::{GuestFixtures, Hypervisor};
    let mut s = support::sandbox("signed_boot::signed_artifact_runs_an_offline_session");
    let a = support::artifact(&mut s);
    s.require::<Hypervisor>();
    let fixtures = s.require::<GuestFixtures>();
    s.note(
        "fixture_guest-oci-base_sha256",
        &awman_test_support::fixtures::sha256_file(&fixtures.paths["guest-oci-base"]).unwrap(),
    );
    support::verify_signature(&s, &a);
    support::session::boot(&s, &a.exe, &fixtures.paths["guest-oci-base"]);
    support::no_helpers(&s);
}
#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn signed_artifact_upgrades_previous_state_offline() {
    published_upgrade("signed_boot::signed_artifact_upgrades_previous_state_offline");
}

/// Selected only when the manifest pins a published Linux builtin predecessor.
#[test]
#[cfg(target_os = "linux")]
fn linux_published_previous_state_upgrades_offline() {
    published_upgrade("signed_boot::linux_published_previous_state_upgrades_offline");
}

fn published_upgrade(id: &'static str) {
    use awman_test_support::capability::{GuestFixtures, Hypervisor, PreviousRelease};
    let mut s = support::sandbox(id);
    let a = support::artifact(&mut s);
    s.require::<Hypervisor>();
    let fixtures = s.require::<GuestFixtures>();
    s.note(
        "fixture_guest-oci-base_sha256",
        &awman_test_support::fixtures::sha256_file(&fixtures.paths["guest-oci-base"]).unwrap(),
    );
    let previous = s.require::<PreviousRelease>();
    s.grant(&previous);
    s.note(
        "previous_release_sha256",
        &awman_test_support::fixtures::sha256_file(&previous.exe).unwrap(),
    );
    #[cfg(target_os = "macos")]
    support::verify_signature(&s, &a);
    let local = s.work().join("source.tar");
    std::fs::copy(&fixtures.paths["guest-oci-base"], &local).unwrap();
    let repo = support::session::prepare(
        &s,
        &local,
        "claude",
        "#!/bin/sh\nIFS= read -r p\necho upgrade-uid=$(id -u)\necho $p\n",
    );
    let saved = support::session::previous_schema(&s, &previous.exe, &repo, &local);
    support::session::ready(&s, &previous.exe, &repo);
    let old = support::session::exec(&s, &previous.exe, &repo, "claude", "before");
    old.assert_success();
    assert!(old.stdout_text().contains("upgrade-uid=1234"));
    std::fs::remove_file(&local).unwrap();
    support::session::restore_schema(&s, &repo, &local, saved);
    support::session::ready(&s, &a.exe, &repo);
    let new = support::session::exec(&s, &a.exe, &repo, "claude", "after");
    new.assert_success();
    assert!(new.stdout_text().contains("upgrade-uid=1234"));
    assert!(new.stdout_text().contains("after"));
    let info = support::info(&s, &a);
    assert_eq!(
        std::fs::read_to_string(support::state(&s).join("awman-runtime-version"))
            .unwrap()
            .trim(),
        info["worker_protocol"].as_str().unwrap()
    );
    support::no_helpers(&s);
}

#[test]
#[cfg(target_os = "linux")]
fn linux_artifact_runs_an_offline_session() {
    use awman_test_support::capability::{GuestFixtures, Hypervisor};
    let mut s = support::sandbox("signed_boot::linux_artifact_runs_an_offline_session");
    let a = support::artifact(&mut s);
    s.require::<Hypervisor>();
    let fixtures = s.require::<GuestFixtures>();
    s.note(
        "fixture_guest-oci-base_sha256",
        &awman_test_support::fixtures::sha256_file(&fixtures.paths["guest-oci-base"]).unwrap(),
    );
    support::session::boot(&s, &a.exe, &fixtures.paths["guest-oci-base"]);
    support::no_helpers(&s);
}

/// First Linux builtin release: an atomic executable replacement must retain
/// cached images and boot offline. This is not historical migration evidence.
#[test]
#[cfg(target_os = "linux")]
fn exact_linux_candidate_replacement_preserves_cached_state_offline() {
    use awman_test_support::capability::{GuestFixtures, Hypervisor};
    let mut s = support::sandbox(
        "signed_boot::exact_linux_candidate_replacement_preserves_cached_state_offline",
    );
    let a = support::artifact(&mut s);
    s.require::<Hypervisor>();
    let fixtures = s.require::<GuestFixtures>();
    let local = s.work().join("source.tar");
    std::fs::copy(&fixtures.paths["guest-oci-base"], &local).unwrap();
    let installed = s.root().join("installed-awman");
    std::fs::copy(&a.exe, &installed).unwrap();
    s.register_exe("installed-awman", &installed);
    let repo = support::session::prepare(
        &s,
        &local,
        "claude",
        "#!/bin/sh\nIFS= read -r prompt\necho REPLACEMENT_GUEST_UID=$(id -u)\necho $prompt\n",
    );
    support::session::ready(&s, &installed, &repo);
    let before = support::session::exec(&s, &installed, &repo, "claude", "before-replace");
    before.assert_success();
    assert!(before.stdout_text().contains("REPLACEMENT_GUEST_UID=1234"));
    let replacement = s.root().join("replacement-awman");
    std::fs::copy(&a.exe, &replacement).unwrap();
    std::fs::rename(replacement, &installed).unwrap();
    std::fs::remove_file(local).unwrap();
    let after = support::session::exec(&s, &installed, &repo, "claude", "after-replace");
    after.assert_success();
    assert!(after.stdout_text().contains("REPLACEMENT_GUEST_UID=1234"));
    assert!(after.stdout_text().contains("after-replace"));
    assert_eq!(
        awman_test_support::fixtures::sha256_file(&installed).unwrap(),
        a.sha256
    );
    support::no_helpers(&s);
}
