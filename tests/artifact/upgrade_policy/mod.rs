use crate::support;
use awman_test_support::capability::PreviousRelease;
#[test]
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
