use super::*;

#[test]
fn sandbox_names_are_stable_short_ascii_and_distinct_per_awman_name() {
    let a = sandbox_name_for("awman-squad-team-a1b2c3d4");
    assert_eq!(a, sandbox_name_for("awman-squad-team-a1b2c3d4"));
    assert_ne!(a, sandbox_name_for("awman-squad-team-a1b2c3d5"));
    assert!(a.starts_with("aw") && a.len() == 16);
    assert!(a
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
}

#[test]
fn every_reserved_label_is_refused_and_caller_labels_survive() {
    for key in [
        LABEL_AWMAN,
        LABEL_NAME,
        LABEL_OWNER,
        LABEL_PROTOCOL,
        LABEL_OWNER_PID,
        LABEL_VERSION,
        LABEL_VCPUS,
        LABEL_MEMORY_MIB,
        LABEL_NETWORK_MODE,
    ] {
        assert!(
            labels("n", "o", TEST_LAUNCH, &[(key.into(), "x".into())]).is_err(),
            "{key} must be reserved"
        );
    }
    let ok = labels(
        "awman-x",
        "tok",
        TEST_LAUNCH,
        &[("awman.session".into(), "s1".into())],
    )
    .unwrap();
    assert_eq!(ok["awman.session"], "s1");
    assert_eq!(ok[LABEL_AWMAN], "true");
    assert_eq!(ok[LABEL_NAME], "awman-x");
    assert_eq!(ok[LABEL_OWNER], "tok");
    assert_eq!(ok[LABEL_PROTOCOL], PROTOCOL);
}

#[test]
fn no_label_value_carries_environment_or_secret_material() {
    let ok = labels("awman-x", "tok", TEST_LAUNCH, &[]).unwrap();
    let joined: String = ok.iter().map(|(k, v)| format!("{k}={v};")).collect();
    for needle in ["KEY", "TOKEN", "SECRET", "PASSWORD", "HOME="] {
        assert!(!joined.contains(needle), "{needle} in {joined}");
    }
}

#[test]
fn a_missing_owner_label_is_not_an_owner() {
    let mut map = labels("awman-x", "tok", TEST_LAUNCH, &[]).unwrap();
    map.remove(LABEL_OWNER);
    assert!(check_owner(&map, "tok").is_err());
    assert!(check_owner(&map, "").is_err());
}
