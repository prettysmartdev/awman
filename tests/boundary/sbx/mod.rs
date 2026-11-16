//! Boundary tests: the Docker Sandboxes `sbx` CLI. macOS only today
//! (`SandboxRuntime::dsbx_in` refuses Linux until the upstream virtiofs
//! file-creation bug is fixed); elsewhere the tests are listed and counted
//! as `not-applicable`, never silently absent.

use crate::support;
use awman::data::message::RecordingMessageSink;
use awman::engine::sandbox::SandboxRuntime;
use awman_test_support::capability::DockerSbx;

/// Every agent in the kit catalogue: `ready_agent` emits its kit, has
/// `sbx kit validate` accept it, and reports no error. Replaces the legacy
/// `sbx_kit_validate_passes_for_all_agents`, which asserted nothing.
const AGENTS: &[&str] = &[
    "antigravity",
    "claude",
    "cline",
    "codex",
    "copilot",
    "gemini",
    "opencode",
];

#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "not-applicable: Docker Sandboxes run only on macOS (dsbx_in refuses Linux)"
)]
fn every_agent_kit_is_emitted_and_validated_by_sbx() {
    let mut sandbox = support::sandbox("sbx::every_agent_kit_is_emitted_and_validated_by_sbx");
    let sbx = sandbox.require::<DockerSbx>();
    sandbox.grant(&sbx);
    let runtime = SandboxRuntime::dsbx_in(support::env_snapshot(&sandbox))
        .unwrap_or_else(|e| panic!("the sandbox runtime was refused on this platform: {e}"));
    use awman::engine::agent_runtime::AgentRuntimeEngine as _;
    assert!(
        runtime.is_available(),
        "BLOCKED[docker-sbx]: `sbx ls` failed; sbx is not installed, logged in or running"
    );

    for agent in AGENTS {
        let mut sink = RecordingMessageSink::new();
        runtime
            .ready_agent(agent, false, &mut sink)
            .unwrap_or_else(|e| panic!("{agent}: kit emission or validation failed: {e}"));
    }
    sandbox.finish();
}

/// WI 0152: Docker Sandboxes share the workspace read-write, so the runtime
/// reports that it cannot mount it read-only (which makes every command
/// refuse `--read-only-workspace` instead of launching with a writable
/// workspace) and never advertises the capability.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "not-applicable: Docker Sandboxes run only on macOS (dsbx_in refuses Linux)"
)]
fn sbx_reports_that_it_cannot_mount_the_workspace_read_only() {
    use awman::engine::agent_runtime::AgentRuntimeEngine as _;
    let mut sandbox =
        support::sandbox("sbx::sbx_reports_that_it_cannot_mount_the_workspace_read_only");
    let sbx = sandbox.require::<DockerSbx>();
    sandbox.grant(&sbx);
    let runtime = SandboxRuntime::dsbx_in(support::env_snapshot(&sandbox))
        .unwrap_or_else(|e| panic!("the sandbox runtime was refused on this platform: {e}"));
    assert!(
        runtime.is_available(),
        "BLOCKED[docker-sbx]: `sbx ls` failed; sbx is not installed, logged in or running"
    );
    assert!(!runtime.capabilities().read_only_workspace());
    assert!(
        !awman::command::daemon_api::capabilities::advertised_for_runtime(
            runtime.capabilities(),
            &[]
        )
        .iter()
        .any(|c| c == "read_only_workspace")
    );
    sandbox.finish();
}
