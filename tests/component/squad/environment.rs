use crate::support;
use awman::data::{
    config::env::DaemonEnvMap,
    fs::daemon_env::{EnvPersistence, NoStore},
};
use awman::engine::squad::env_state::{coverage_digest, DaemonEnvState, Salt};
use std::collections::BTreeMap;
#[test]
fn env_sync_accepts_only_required_names_and_keeps_each_daemon_private() {
    let sandbox = support::sandbox(
        "squad::environment::env_sync_accepts_only_required_names_and_keeps_each_daemon_private",
    );
    let state =
        || DaemonEnvState::with_payload(Box::new(NoStore), EnvPersistence::None, Salt::random());
    let a = state();
    let b = state();
    let now = chrono::Utc::now();
    for state in [&a, &b] {
        state.set_required(BTreeMap::from([("TOKEN".into(), vec!["task".into()])]), now);
    }
    let (accepted, ignored) = a.apply_push(
        DaemonEnvMap::from_pairs([("TOKEN", "private-value"), ("UNASKED", "discard")]),
        &[],
        now,
    );
    assert_eq!(accepted, vec!["TOKEN"]);
    assert_eq!(ignored, vec!["UNASKED"]);
    assert_eq!(
        a.entries()[0].digest,
        Some(coverage_digest(&a.salt(), "TOKEN", "private-value"))
    );
    assert!(b.entries()[0].digest.is_none());
    assert!(!serde_json::to_string(&a.entries())
        .unwrap()
        .contains("private-value"));
    a.set_required(BTreeMap::new(), now);
    assert!(a.payload_snapshot().get("TOKEN").is_none());
    sandbox.finish();
}
