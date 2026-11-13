//! Frontend parity over both real backend entry points.
use crate::support::{
    self,
    backend::{Backend, BackendKind},
};
use awman_test_support::Exit;

async fn config_round_trip(kind: BackendKind, id: &'static str) -> String {
    let s = support::sandbox(id);
    let backend = Backend::start(kind, &s).await;
    let set = support::awman(&s)
        .envs(&backend.env())
        .args(["config", "set", "agents.stuckTimeout", "71s"])
        .run();
    set.assert_success();
    let get = support::awman(&s)
        .envs(&backend.env())
        .args(["config", "get", "agents.stuckTimeout"])
        .run();
    get.assert_success();
    assert!(get.stdout_text().contains("71"), "{}", get.stdout_text());
    let invalid = support::awman(&s)
        .envs(&backend.env())
        .args(["config", "set", "agents.stuckTimeout", "nonsense"])
        .run();
    assert_ne!(invalid.exit, Exit::Code(0));
    let unchanged = support::awman(&s)
        .envs(&backend.env())
        .args(["config", "get", "agents.stuckTimeout"])
        .run();
    unchanged.assert_success();
    assert_eq!(get.stdout_text(), unchanged.stdout_text());
    let result = get.stdout_text();
    backend.shutdown().await;
    s.finish();
    result
}
mod builtin {
    #[tokio::test]
    async fn config_mutations_are_durable_and_invalid_edits_are_atomic() {
        assert!(super::config_round_trip(
            super::BackendKind::Builtin,
            "parity::builtin::config_mutations_are_durable_and_invalid_edits_are_atomic"
        )
        .await
        .contains("71"));
    }
}
mod daemon {
    #[tokio::test]
    async fn config_mutations_are_durable_and_invalid_edits_are_atomic() {
        assert!(super::config_round_trip(
            super::BackendKind::Daemon,
            "parity::daemon::config_mutations_are_durable_and_invalid_edits_are_atomic"
        )
        .await
        .contains("71"));
    }
}
mod differential {
    #[tokio::test]
    async fn config_results_agree_across_backends() {
        let builtin =
            super::config_round_trip(super::BackendKind::Builtin, "parity::differential::builtin")
                .await;
        let daemon =
            super::config_round_trip(super::BackendKind::Daemon, "parity::differential::daemon")
                .await;
        assert_eq!(builtin, daemon);
    }
}
