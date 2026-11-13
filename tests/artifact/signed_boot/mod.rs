//! not-applicable(linux,windows): shipped signed builtin sessions require native Apple Silicon.
use crate::support;
#[test]
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    ignore = "not-applicable: signed Hypervisor.framework session needs native Apple Silicon"
)]
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
#[cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    ignore = "not-applicable: signed Hypervisor.framework upgrade needs native Apple Silicon"
)]
fn signed_artifact_upgrades_previous_state_offline() {
    use awman_test_support::capability::{GuestFixtures, Hypervisor, PreviousRelease};
    let mut s = support::sandbox("signed_boot::signed_artifact_upgrades_previous_state_offline");
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
