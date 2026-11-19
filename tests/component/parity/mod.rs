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
    async fn plan_and_mount_access_reach_the_final_launch_independently() {
        super::agents::plan_and_mount_access_reach_the_final_launch_independently(
            super::BackendKind::Builtin,
            "parity::builtin::plan_and_mount_access_reach_the_final_launch_independently",
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn cursor_env_output_is_redacted() {
        super::agents::cursor_env_output_is_redacted(
            super::BackendKind::Builtin,
            "parity::builtin::cursor_env_output_is_redacted",
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn opencode_selects_its_own_definition() {
        super::agents::agent_selects_its_own_definition(
            super::BackendKind::Builtin,
            "parity::builtin::opencode_selects_its_own_definition",
            "opencode",
            true,
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn opencode_v2_selects_its_own_definition() {
        super::agents::agent_selects_its_own_definition(
            super::BackendKind::Builtin,
            "parity::builtin::opencode_v2_selects_its_own_definition",
            "opencode-v2",
            true,
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn cursor_selects_its_own_definition() {
        super::agents::agent_selects_its_own_definition(
            super::BackendKind::Builtin,
            "parity::builtin::cursor_selects_its_own_definition",
            "cursor",
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
    async fn opencode_selects_its_own_definition() {
        super::agents::agent_selects_its_own_definition(
            super::BackendKind::Daemon,
            "parity::daemon::opencode_selects_its_own_definition",
            "opencode",
            true,
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn opencode_v2_selects_its_own_definition() {
        super::agents::agent_selects_its_own_definition(
            super::BackendKind::Daemon,
            "parity::daemon::opencode_v2_selects_its_own_definition",
            "opencode-v2",
            true,
        )
        .await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn cursor_selects_its_own_definition() {
        super::agents::agent_selects_its_own_definition(
            super::BackendKind::Daemon,
            "parity::daemon::cursor_selects_its_own_definition",
            "cursor",
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
    async fn agent_version_readiness_agrees_across_backends() {
        let builtin = super::agents::agent_version_readiness_contract(
            super::BackendKind::Builtin,
            "parity::differential::readiness_builtin",
        )
        .await;
        let daemon = super::agents::agent_version_readiness_contract(
            super::BackendKind::Daemon,
            "parity::differential::readiness_daemon",
        )
        .await;
        assert_eq!(builtin, daemon);
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn opencode_launches_agree_across_backends() {
        use super::agents::agent_selects_its_own_definition as selected;
        use super::BackendKind::{Builtin, Daemon};
        let builtin = selected(
            Builtin,
            "parity::differential::opencode_builtin",
            "opencode",
            false,
        )
        .await;
        let daemon = selected(
            Daemon,
            "parity::differential::opencode_daemon",
            "opencode",
            false,
        )
        .await;
        assert_eq!(builtin, daemon);
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn opencode_v2_launches_agree_across_backends() {
        use super::agents::agent_selects_its_own_definition as selected;
        use super::BackendKind::{Builtin, Daemon};
        let builtin = selected(
            Builtin,
            "parity::differential::opencode_v2_builtin",
            "opencode-v2",
            false,
        )
        .await;
        let daemon = selected(
            Daemon,
            "parity::differential::opencode_v2_daemon",
            "opencode-v2",
            false,
        )
        .await;
        assert_eq!(builtin, daemon);
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn cursor_launches_agree_across_backends() {
        use super::agents::agent_selects_its_own_definition as selected;
        use super::BackendKind::{Builtin, Daemon};
        let builtin = selected(
            Builtin,
            "parity::differential::cursor_builtin",
            "cursor",
            false,
        )
        .await;
        let daemon = selected(
            Daemon,
            "parity::differential::cursor_daemon",
            "cursor",
            false,
        )
        .await;
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
