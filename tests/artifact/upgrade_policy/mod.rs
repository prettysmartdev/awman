use crate::support;
#[cfg(not(target_os = "linux"))]
use awman_test_support::capability::PreviousRelease;
#[test]
#[cfg(not(target_os = "linux"))]
#[cfg_attr(
    windows,
    ignore = "not-applicable: Windows does not ship the builtin worker protocol"
)]
fn published_previous_protocol_is_accepted_by_exact_candidate() {
    let mut s = support::sandbox(
        "upgrade_policy::published_previous_protocol_is_accepted_by_exact_candidate",
    );
    let a = support::artifact(&mut s);
    let previous = s.require::<PreviousRelease>();
    s.grant(&previous);
    let old = support::run_exact(&s, &previous.exe)
        .arg("__awman-builtin-info")
        .run();
    old.assert_success();
    let old: serde_json::Value = serde_json::from_slice(&old.stdout).unwrap();
    let current = support::info(&s, &a);
    let protocol = old["worker_protocol"]
        .as_str()
        .expect("published previous protocol");
    assert!(
        current["worker_protocol"] == protocol
            || current["upgradable_from"]
                .as_array()
                .expect("upgrade policy must be retained in artifact")
                .iter()
                .any(|v| v == protocol),
        "candidate does not accept published protocol {protocol}: {current}"
    );
}

/// The first Linux builtin candidate has no published builtin predecessor.
/// Its retained worker protocol policy must still be complete and unambiguous.
#[test]
#[cfg(target_os = "linux")]
fn linux_candidate_retains_an_unambiguous_worker_protocol_policy() {
    let mut s = support::sandbox(
        "upgrade_policy::linux_candidate_retains_an_unambiguous_worker_protocol_policy",
    );
    let a = support::artifact(&mut s);
    let info = support::info(&s, &a);
    let current = info["worker_protocol"]
        .as_str()
        .expect("worker protocol retained");
    assert!(!current.is_empty());
    let predecessors = info["upgradable_from"]
        .as_array()
        .expect("upgrade policy retained");
    let mut unique = std::collections::BTreeSet::new();
    for value in predecessors {
        let protocol = value.as_str().expect("protocol string");
        assert!(!protocol.is_empty());
        assert_ne!(protocol, current);
        assert!(
            unique.insert(protocol),
            "duplicate upgrade protocol {protocol}"
        );
    }
}
