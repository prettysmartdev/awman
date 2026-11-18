//! Frontend parity over both real backend entry points.
mod agents;
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
    #[tokio::test(flavor = "multi_thread")]
    async fn cursor_env_output_is_redacted() {
        super::agents::cursor_env_output_is_redacted(
            super::BackendKind::Builtin,
            "parity::builtin::cursor_env_output_is_redacted",
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn pinned_agents_select_their_own_definition() {
        super::agents::pinned_agents_select_their_own_definition(
            super::BackendKind::Builtin,
            "parity::builtin::pinned_agents_select_their_own_definition",
            true,
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn retired_gemini_is_refused_before_build_or_launch() {
        super::agents::retired_gemini_is_refused_before_build_or_launch(
            super::BackendKind::Builtin,
            "parity::builtin::retired_gemini_is_refused_before_build_or_launch",
        )
        .await;
    }
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
    #[tokio::test(flavor = "multi_thread")]
    async fn pinned_agents_select_their_own_definition() {
        super::agents::pinned_agents_select_their_own_definition(
            super::BackendKind::Daemon,
            "parity::daemon::pinned_agents_select_their_own_definition",
            true,
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn retired_gemini_is_refused_before_build_or_launch() {
        super::agents::retired_gemini_is_refused_before_build_or_launch(
            super::BackendKind::Daemon,
            "parity::daemon::retired_gemini_is_refused_before_build_or_launch",
        )
        .await;
    }
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
    #[tokio::test(flavor = "multi_thread")]
    async fn pinned_agent_readiness_agrees_across_backends() {
        let builtin = super::agents::pinned_readiness_contract(
            super::BackendKind::Builtin,
            "parity::differential::readiness_builtin",
        )
        .await;
        let daemon = super::agents::pinned_readiness_contract(
            super::BackendKind::Daemon,
            "parity::differential::readiness_daemon",
        )
        .await;
        assert_eq!(builtin, daemon);
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn pinned_agent_launches_agree_across_backends() {
        use super::agents::pinned_agents_select_their_own_definition as pinned;
        use super::BackendKind::{Builtin, Daemon};
        let builtin = pinned(Builtin, "parity::differential::agents_builtin", false).await;
        let daemon = pinned(Daemon, "parity::differential::agents_daemon", false).await;
        assert_eq!(builtin, daemon);
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn retired_gemini_refusals_agree_across_backends() {
        use super::agents::retired_gemini_is_refused_before_build_or_launch as gemini;
        use super::BackendKind::{Builtin, Daemon};
        let builtin = gemini(Builtin, "parity::differential::gemini_builtin").await;
        let daemon = gemini(Daemon, "parity::differential::gemini_daemon").await;
        assert_eq!(builtin, daemon);
    }
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
