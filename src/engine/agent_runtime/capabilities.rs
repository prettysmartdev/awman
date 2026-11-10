//! `Capabilities` — static description of what an `AgentRuntimeEngine` can do.
//!
//! Layer 2 reads these flags to decide how to map cross-paradigm options
//! before calling `build()`, instead of matching on concrete runtime types.
//!
//! Every flag here has a production reader. A flag that no decision branches
//! on is deleted rather than kept as documentation (WI 0126 C7 removed
//! `arbitrary_env_vars`, `cpu_limits`, `fractional_cpu`, `per_resource_stats`,
//! `persistent_lifecycle`, `host_paths_visible`, `session_label_supported` and
//! `dind`). Where a runtime cannot honour a request (builtin and
//! `--allow-docker`, fractional `--cpus`), its backend refuses the launch with
//! a typed error; that is the single enforcement point.

/// Paradigm-specific flags Layer 2 can branch on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Arbitrary host paths can be bind-mounted.
    /// true: container; false: sandbox. Read by [`Self::squad_supported`].
    pub arbitrary_host_mounts: bool,
    /// The agent environment is declared via a kit/template rather than a
    /// locally-built image. false: container; true: sandbox. Read by the
    /// chat/exec/workflow commands to pick the option paradigm.
    pub kit_declarative: bool,
    /// The runtime keeps a local image store that can accumulate dangling
    /// images and needs reclaiming. true: container; false: sandbox, whose
    /// agent environments are declared by kit and have no image layer cache
    /// of awman's own to sweep. Read by `awman clean`.
    pub has_image_store: bool,
    /// How the runtime obtains an agent image. `Build`: container (Docker,
    /// Apple); `Import`: builtin; `Kit`: sandbox.
    ///
    /// Static by design. What a runtime *can* do is a capability; what a
    /// session *will* do for a given image (build locally, import, or warn
    /// that the source is incompatible with the local builder) depends on
    /// that session's config and the image's resolved source, and is decided
    /// per image by the preparer's `BuilderSelection`
    /// (`engine::oci::resolve`). Nothing folds session config into this
    /// value; a caller that needs the per-image answer asks the preparer.
    pub image_acquisition: ImageAcquisition,
    /// Each launch is sized and confined by its own session's
    /// `BuiltinLaunchSettings` (network policy, default vCPUs and memory),
    /// which the backend refuses to launch without. true: builtin; false:
    /// Docker, Apple, sandbox, which neither read nor validate a `builtin`
    /// block. Read by `AgentEngine::builtin_launch_settings`.
    pub per_launch_vm_settings: bool,
    /// Whether `stats()` returns measured per-agent CPU/memory. `false` means
    /// the runtime answers zeros it did not measure (Docker Sandboxes), so
    /// callers show "not reported" instead of `0.0%`. Read by
    /// `execution::default_detail` and by the agents commands that collect
    /// stats.
    pub reports_resource_stats: bool,
}

impl Capabilities {
    /// Whether squad can run tasks on this runtime.
    ///
    /// squad binds a task's persistent workspace and per-run context
    /// directories into the leader container and runs workflow setup/teardown
    /// steps against the host tree, so a runtime that cannot mount arbitrary
    /// host paths cannot host a task at all. Reading it off the capability
    /// rather than the concrete tier is what lets Layer 1 assert the same
    /// admission rule Layer 2 enforces before bootstrap (WI 0113 F-02).
    pub fn squad_supported(&self) -> bool {
        self.arbitrary_host_mounts
    }

    /// Whether the runtime can mount the session workspace read-only while
    /// keeping a scratch directory and the agent home writable. Every
    /// container-tier driver (Docker, Apple, builtin) honours a read-only
    /// bind mount; the sandbox tier shares its workspace directory
    /// read-write and cannot. Read by `AgentEngine` (which refuses a
    /// read-only launch it cannot enforce) and by the worker's lease
    /// eligibility check and WSP registration (`lease_read_only`).
    pub fn read_only_workspace(&self) -> bool {
        self.arbitrary_host_mounts
    }

    /// Whether a requested `/awman/output` directory is actually mounted on
    /// this runtime (`outputs_mounted` of a command result and WSP lease
    /// result). `requested` is whether the command was given an output
    /// directory at all.
    pub fn mounts_outputs(&self, requested: bool) -> bool {
        requested && self.arbitrary_host_mounts
    }
}
/// How a runtime obtains the image an agent runs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageAcquisition {
    /// Built locally from a Dockerfile by the runtime (`build_image`).
    Build,
    /// Imported from an explicit, already-built source (`import_image`).
    Import,
    /// Declared by a kit or template; no image of awman's own (`ready_agent`).
    Kit,
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
            caps.arbitrary_host_mounts,
            "docker supports arbitrary host mounts"
        );
        assert!(!caps.kit_declarative, "docker uses local images, not kits");
        assert!(caps.has_image_store, "docker keeps a local image store");
        assert_eq!(caps.image_acquisition, ImageAcquisition::Build);
        assert!(caps.reports_resource_stats);
        assert!(caps.squad_supported());
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
        // SandboxRuntime::dsbx() errors on Linux — that platform
        // verifies the guard error instead.
        use crate::engine::sandbox::SandboxRuntime;
        match SandboxRuntime::dsbx() {
            Ok(rt) => {
                let caps = rt.capabilities();
                assert!(
                    !caps.arbitrary_host_mounts,
                    "sandbox cannot bind arbitrary paths"
                );
                assert!(caps.kit_declarative, "sandbox uses kit/template");
                assert!(
                    !caps.has_image_store,
                    "sandbox has no local image store to reclaim"
                );
                assert_eq!(caps.image_acquisition, ImageAcquisition::Kit);
                assert!(
                    !caps.reports_resource_stats,
                    "sandbox stats are zeros the runtime did not measure"
                );
                assert!(!caps.squad_supported());
            }
            Err(_) => {
                // Unsupported platform — platform guard test covers this branch.
            }
        }
    }
}

#[cfg(test)]
mod lease_capability_tests {
    use crate::engine::agent_runtime::AgentRuntimeEngine;
    use crate::engine::container::ContainerRuntime;

    #[test]
    fn container_tier_supports_read_only_workspace_and_outputs_mount() {
        for rt in [ContainerRuntime::docker(), ContainerRuntime::apple()] {
            let caps = rt.capabilities();
            assert!(caps.read_only_workspace());
            assert!(caps.mounts_outputs(true));
            assert!(!caps.mounts_outputs(false));
        }
        if let Ok(rt) = crate::engine::sandbox::SandboxRuntime::dsbx() {
            let caps = rt.capabilities();
            assert!(!caps.read_only_workspace());
            assert!(!caps.mounts_outputs(true));
        }
    }
}
