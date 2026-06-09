//! `Capabilities` — static description of what an `AgentRuntimeEngine` can do.
//!
//! Layer 2 reads these flags to decide how to map cross-paradigm options
//! before calling `build()`, instead of matching on concrete runtime types.

/// Paradigm-specific flags Layer 2 can branch on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Arbitrary `-e KEY=VALUE` env vars can be injected at launch.
    /// true: container; false: sandbox.
    pub arbitrary_env_vars: bool,
    /// Arbitrary host paths can be bind-mounted.
    /// true: container; false: sandbox.
    pub arbitrary_host_mounts: bool,
    /// CPU limits can be applied per agent.
    /// true: container; false: sandbox.
    pub cpu_limits: bool,
    /// Per-handle CPU/memory stats are available.
    /// true: container; false: sandbox.
    pub per_resource_stats: bool,
    /// The agent environment persists across runs (stop preserves state).
    /// false: container; true: sandbox.
    pub persistent_lifecycle: bool,
    /// The agent environment is declared via a kit/template rather than a
    /// locally-built image. false: container; true: sandbox.
    pub kit_declarative: bool,
    /// Docker-in-Docker support model.
    pub dind: DindSupport,
    /// Host paths outside the workspace are visible to the agent.
    /// true: container; false: sandbox (workspace only).
    pub host_paths_visible: bool,
    /// Agents can be attributed to a session via a runtime label.
    /// true: container; false: sandbox (uses names).
    pub session_label_supported: bool,
}

/// How a runtime provides Docker-in-Docker to its agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DindSupport {
    /// Every sandbox VM has a private DinD daemon.
    Always,
    /// Available when explicitly requested (`--allow-docker`).
    OnRequest,
    /// Not available.
    Never,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent_runtime::AgentRuntimeEngine;
    use crate::engine::container::ContainerRuntime;

    // ─── Container capabilities ───────────────────────────────────────────────

    #[test]
    fn docker_runtime_capabilities_match_expected() {
        let rt = ContainerRuntime::docker();
        let caps = rt.capabilities();
        assert!(
            caps.arbitrary_env_vars,
            "docker supports arbitrary env vars"
        );
        assert!(
            caps.arbitrary_host_mounts,
            "docker supports arbitrary host mounts"
        );
        assert!(caps.cpu_limits, "docker supports cpu limits");
        assert!(
            caps.per_resource_stats,
            "docker supports per-resource stats"
        );
        assert!(!caps.persistent_lifecycle, "docker is ephemeral");
        assert!(!caps.kit_declarative, "docker uses local images, not kits");
        assert_eq!(
            caps.dind,
            DindSupport::OnRequest,
            "docker dind is on-request"
        );
        assert!(caps.host_paths_visible, "docker can mount host paths");
        assert!(
            caps.session_label_supported,
            "docker supports session labels"
        );
    }

    #[test]
    fn apple_runtime_capabilities_same_as_docker() {
        // Apple Containers share container-paradigm capabilities with Docker.
        let docker = ContainerRuntime::docker();
        let apple = ContainerRuntime::apple();
        assert_eq!(docker.capabilities(), apple.capabilities());
    }

    // ─── Sandbox capabilities ─────────────────────────────────────────────────

    #[test]
    fn sandbox_runtime_capabilities_match_expected() {
        // SandboxRuntime::dsbx() errors on Linux and Intel-Mac — those platforms
        // verify the guard error instead.
        use crate::engine::sandbox::SandboxRuntime;
        match SandboxRuntime::dsbx() {
            Ok(rt) => {
                let caps = rt.capabilities();
                assert!(
                    !caps.arbitrary_env_vars,
                    "sandbox cannot inject arbitrary env"
                );
                assert!(
                    !caps.arbitrary_host_mounts,
                    "sandbox cannot bind arbitrary paths"
                );
                assert!(!caps.cpu_limits, "sandbox does not enforce cpu limits");
                assert!(
                    !caps.per_resource_stats,
                    "sandbox has no per-resource stats"
                );
                assert!(caps.persistent_lifecycle, "sandbox is persistent");
                assert!(caps.kit_declarative, "sandbox uses kit/template");
                assert_eq!(caps.dind, DindSupport::Always, "sandbox always has dind");
                assert!(!caps.host_paths_visible, "sandbox cannot see host paths");
                assert!(
                    !caps.session_label_supported,
                    "sandbox uses names, not labels"
                );
            }
            Err(_) => {
                // Unsupported platform — platform guard test covers this branch.
            }
        }
    }
}
