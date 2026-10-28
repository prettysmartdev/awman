//! Merged configuration view: flags > env > repo > global > built-in default.
//!
//! Every legacy `effective_*` free function in `oldsrc/config/mod.rs` becomes a
//! method on `EffectiveConfig`. The merge precedence is encoded once, in this
//! module, and is the single source of truth.

use std::path::PathBuf;
use std::time::Duration;

use crate::data::config::builtin_launch::BuiltinLaunchSettings;
use crate::data::config::builtin_network::{BuiltinNetworkConfig, BuiltinNetworkSettings};
use crate::data::config::builtin_runtime::{BuiltinRuntimeConfig, DEFAULT_STATE_SUBDIR};
use crate::data::config::env::EnvSnapshot;
use crate::data::config::flags::FlagConfig;
use crate::data::config::global::{GlobalConfig, LaunchModeFallback};
use crate::data::config::repo::{
    AgentAuthMode, LaunchMode, OnDirtyMergeTarget, OnMergeFailure, RepoConfig, WorktreeConfig,
};
use crate::data::config::resolved_builtin::ResolvedBuiltinConfig;
use crate::data::config::runtime_selection::{RuntimeSelection, UnknownRuntimeValue};
use crate::data::config::{DEFAULT_AGENT_STUCK_TIMEOUT_SECS, DEFAULT_SCROLLBACK_LINES};
use crate::data::error::DataError;

/// Merged view of every configuration source, in precedence order.
///
/// The fields are intentionally `pub(crate)`-free; access goes through
/// dedicated methods so that callers cannot accidentally bypass the merge.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    flags: FlagConfig,
    env: EnvSnapshot,
    repo: RepoConfig,
    global: GlobalConfig,
}

/// Resolved live credential-refresh settings. Repository fields override the
/// corresponding global fields; absent values use the built-in defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthRefreshSettings {
    pub enabled: bool,
    pub threshold: Duration,
    pub tick: Duration,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self::new(
            FlagConfig::default(),
            crate::data::config::env::EnvSnapshot::default(),
            RepoConfig::default(),
            GlobalConfig::default(),
        )
    }
}

impl EffectiveConfig {
    /// Construct a merged view from the four source layers.
    ///
    /// Precedence (highest → lowest): `flags` > `env` > `repo` > `global` > built-in.
    pub fn new(
        flags: FlagConfig,
        env: EnvSnapshot,
        repo: RepoConfig,
        global: GlobalConfig,
    ) -> Self {
        Self {
            flags,
            env,
            repo,
            global,
        }
    }

    pub fn flags(&self) -> &FlagConfig {
        &self.flags
    }

    pub fn env(&self) -> &EnvSnapshot {
        &self.env
    }

    pub fn repo(&self) -> &RepoConfig {
        &self.repo
    }

    pub fn global(&self) -> &GlobalConfig {
        &self.global
    }

    /// Resolve the model override (flag only; no repo/global level for model).
    pub fn model(&self) -> Option<String> {
        self.flags.model.clone()
    }

    /// Resolve the agent name (flag > repo.agent > global.default_agent).
    pub fn agent(&self) -> Option<String> {
        if let Some(a) = self.flags.agent.as_deref() {
            return Some(a.to_string());
        }
        if let Some(a) = self.repo.agent.as_deref() {
            return Some(a.to_string());
        }
        self.global.default_agent.clone()
    }

    /// Effective `yoloDisallowedTools` list.
    pub fn yolo_disallowed_tools(&self) -> Vec<String> {
        if let Some(values) = self.flags.yolo_disallowed_tools.as_ref() {
            return values.clone();
        }
        if let Some(values) = self.repo.yolo_disallowed_tools.as_ref() {
            return values.clone();
        }
        if let Some(values) = self.global.yolo_disallowed_tools.as_ref() {
            return values.clone();
        }
        Vec::new()
    }

    /// Effective extra arguments for `agent`'s launch command
    /// (flag > repo entry > global entry > none).
    ///
    /// Each level replaces the lower ones for that agent, so an empty repo
    /// entry turns off a global one. `--agent-flag` targets the agent this
    /// invocation selects ([`Self::agent`]), so it applies only when `agent` is
    /// that one; other agents (e.g. a workflow step that names its own) fall
    /// through to config.
    pub fn agent_flags(&self, agent: &str) -> Vec<String> {
        if let Some(values) = self.flags.agent_flags.as_ref() {
            if self.agent().as_deref() == Some(agent) {
                return values.clone();
            }
        }
        [
            self.repo.agent_flags.as_ref(),
            self.global.agent_flags.as_ref(),
        ]
        .into_iter()
        .flatten()
        .find_map(|map| map.get(agent))
        .cloned()
        .unwrap_or_default()
    }

    /// Effective scrollback line count for the container terminal.
    pub fn scrollback_lines(&self) -> usize {
        if let Some(n) = self.flags.terminal_scrollback_lines {
            return n;
        }
        if let Some(n) = self.repo.terminal_scrollback_lines {
            return n;
        }
        if let Some(n) = self.global.terminal_scrollback_lines {
            return n;
        }
        DEFAULT_SCROLLBACK_LINES
    }

    /// Effective agent-stuck timeout.
    pub fn agent_stuck_timeout(&self) -> Duration {
        if let Some(d) = self.flags.agent_stuck_timeout {
            return d;
        }
        if let Some(secs) = self.repo.agent_stuck_timeout_secs {
            return Duration::from_secs(secs);
        }
        if let Some(secs) = self.global.agent_stuck_timeout_secs {
            return Duration::from_secs(secs);
        }
        Duration::from_secs(DEFAULT_AGENT_STUCK_TIMEOUT_SECS)
    }

    /// Effective API work-dirs allowlist.
    pub fn api_work_dirs(&self) -> Vec<String> {
        if let Some(api_cfg) = self.global.api.as_ref() {
            if let Some(dirs) = api_cfg.work_dirs.as_ref() {
                return dirs.clone();
            }
        }
        Vec::new()
    }

    /// Effective API workdir roots allowlist.
    pub fn api_work_dir_roots(&self) -> Vec<String> {
        if let Some(api_cfg) = self.global.api.as_ref() {
            if let Some(roots) = api_cfg.work_dir_roots.as_ref() {
                return roots.clone();
            }
        }
        Vec::new()
    }

    /// Effective `alwaysNonInteractive` setting.
    pub fn always_non_interactive(&self) -> bool {
        if let Some(value) = self.flags.non_interactive {
            return value;
        }
        self.global
            .api
            .as_ref()
            .and_then(|h| h.always_non_interactive)
            .unwrap_or(false)
    }

    /// Effective remote default address (flag > env > global config).
    pub fn remote_default_addr(&self) -> Option<String> {
        if let Some(v) = self.flags.remote_addr.as_deref() {
            return Some(v.to_string());
        }
        if let Some(v) = self.env.remote_addr() {
            return Some(v.to_string());
        }
        self.global
            .remote
            .as_ref()
            .and_then(|r| r.default_addr.clone())
    }

    /// Effective remote default API key (flag > env > global config).
    pub fn remote_default_api_key(&self) -> Option<String> {
        if let Some(v) = self.flags.api_key.as_deref() {
            return Some(v.to_string());
        }
        if let Some(v) = self.env.api_key() {
            return Some(v.to_string());
        }
        self.global
            .remote
            .as_ref()
            .and_then(|r| r.default_api_key.clone())
    }

    /// Effective remote saved-dirs list.
    pub fn remote_saved_dirs(&self) -> Vec<String> {
        self.global
            .remote
            .as_ref()
            .and_then(|r| r.saved_dirs.clone())
            .unwrap_or_default()
    }

    /// Effective sticky remote session id (flag > env).
    pub fn remote_session(&self) -> Option<String> {
        if let Some(v) = self.flags.remote_session.as_deref() {
            return Some(v.to_string());
        }
        self.env.remote_session().map(|s| s.to_string())
    }

    /// Effective container runtime name (e.g. `"docker"`, `"apple-containers"`).
    pub fn runtime(&self) -> Option<String> {
        self.global.runtime.clone()
    }

    /// Effective agent launch mode (flag > env > repo > built-in `stdio`).
    ///
    /// Launch mode is intentionally not inherited from global configuration;
    /// it is scoped to an invocation or repository. The repository setting is
    /// agent-independent, so no agent name is needed here (the agent-support
    /// check happens separately, in the command layer and `build_options`).
    pub fn launch_mode(&self) -> LaunchMode {
        if let Some(mode) = self.flags.launch_mode {
            return mode;
        }
        if let Some(mode) = self.env.launch_mode() {
            return mode;
        }
        self.repo.launch_mode.unwrap_or_default()
    }

    /// Effective ACP fallback policy. This is global-only; flags, environment
    /// and repository configuration do not override it.
    pub fn launch_mode_fallback(&self) -> LaunchModeFallback {
        self.global.launch_mode_fallback.unwrap_or_default()
    }

    /// Effective runtime selection (global only, like [`Self::runtime`]).
    pub fn runtime_selection(&self) -> Result<RuntimeSelection, UnknownRuntimeValue> {
        self.global.runtime_selection()
    }

    /// Effective builtin-runtime settings: the repo `builtin` block merged over
    /// the global one field by field (map entries per key). Defaults and
    /// validation are applied by Layer 0 in [`Self::builtin_settings`].
    ///
    /// The view never carries `network`: the enforced policy is
    /// [`Self::builtin_network`] (global ceiling, repo may only narrow), and
    /// a merged copy would be a value nobody enforces.
    pub fn builtin_runtime(&self) -> BuiltinRuntimeConfig {
        let global = self.global.builtin.clone().unwrap_or_default();
        let merged = match &self.repo.builtin {
            Some(repo) => repo.merged_over(&global),
            None => global,
        };
        BuiltinRuntimeConfig {
            network: None,
            ..merged
        }
    }

    /// Effective `builtin.builder.enabled`: repo over global per leaf,
    /// default `false`. Whether it applies to a runtime is the engine's
    /// decision.
    #[cfg(test)]
    fn builtin_builder_enabled(&self) -> bool {
        self.builtin_runtime().builder_enabled()
    }

    /// Resolve all builtin-runtime settings for the engine boundary.
    ///
    /// Layer 0 applies defaults, validates resources and image sources,
    /// enforces global network ceilings against repo policy, and supplies the
    /// session's image-acquisition inputs in one typed value.
    pub fn builtin_settings(&self) -> Result<ResolvedBuiltinConfig, DataError> {
        let builtin = self.builtin_runtime();
        builtin
            .validate_image_sources()
            .map_err(DataError::BuiltinConfig)?;
        let launch = self.builtin_launch_settings()?;
        let builder_enabled = builtin.builder_enabled();
        let process = self.builtin_process_settings()?;
        Ok(ResolvedBuiltinConfig {
            state_dir: process.state_dir,
            ambient_overrides: process.ambient_overrides,
            test_isolation: process.test_isolation,
            launch,
            image_source: builtin.image_source,
            images: builtin.images,
            registries: builtin.registries,
            builder_enabled,
        })
    }

    /// Effective builtin network policy: the global `builtin.network` block is
    /// the ceiling and the repo block may only narrow it (see
    /// [`BuiltinNetworkConfig::resolve`]). Errors name the offending key.
    pub fn builtin_network(&self) -> Result<BuiltinNetworkSettings, String> {
        BuiltinNetworkConfig::resolve(
            self.global
                .builtin
                .as_ref()
                .and_then(|b| b.network.as_ref()),
            self.repo.builtin.as_ref().and_then(|b| b.network.as_ref()),
        )
    }

    /// The builtin settings one launch for this session is sized and confined
    /// by: default vCPUs and memory, and the network policy (global ceiling
    /// narrowed by the repo block). Resolved per session at every launch, so
    /// many sessions can share one builtin runtime and each keep its own
    /// repo's values.
    ///
    /// Fails closed: zero resources or a repo network block that would widen
    /// the global one is a [`DataError::BuiltinConfig`] for this launch. A
    /// repo `builtin.stateDir` is resolved (with the usual precedence) into
    /// [`BuiltinLaunchSettings::repo_state_dir`] for the runtime to compare
    /// with its own process-wide directory.
    pub fn builtin_launch_settings(&self) -> Result<BuiltinLaunchSettings, DataError> {
        let builtin = self.builtin_runtime();
        builtin
            .validate_resources()
            .map_err(DataError::BuiltinConfig)?;
        let network = self.builtin_network().map_err(DataError::BuiltinConfig)?;
        let repo_sets_state_dir = self
            .repo
            .builtin
            .as_ref()
            .is_some_and(|b| b.state_dir.is_some());
        let repo_state_dir = if repo_sets_state_dir {
            Some(self.builtin_state_dir()?)
        } else {
            None
        };
        Ok(BuiltinLaunchSettings {
            vcpus: builtin.vcpus_or_default(),
            memory_mib: builtin.memory_mib_or_default(),
            root_disk_mib: builtin.root_disk_mib_or_default(),
            mount_quota_mib: builtin.mount_quota_mib_or_default(),
            network,
            repo_state_dir,
        })
    }

    /// The process-wide builtin settings runtime detection needs. Unlike
    /// [`EffectiveConfig::builtin_settings`] it does not validate the
    /// per-session parts (network narrowing, vCPUs, memory, image sources):
    /// those fail each launch of the repo that sets them, not the process.
    pub fn builtin_process_settings(
        &self,
    ) -> Result<crate::data::config::resolved_builtin::BuiltinProcessConfig, DataError> {
        let env = self.env();
        Ok(
            crate::data::config::resolved_builtin::BuiltinProcessConfig {
                state_dir: self.builtin_state_dir()?,
                ambient_overrides: env.ambient_msb_overrides(),
                test_isolation: crate::data::config::env::test_isolation_active()
                    && !env.test_builtin(),
            },
        )
    }

    /// Effective builtin-runtime state directory:
    /// `AWMAN_BUILTIN_STATE_DIR` > `builtin.stateDir` (repo > global) >
    /// `<data home>/builtin`.
    pub fn builtin_state_dir(&self) -> Result<PathBuf, DataError> {
        if let Some(dir) = self.env.builtin_state_dir() {
            return Ok(dir);
        }
        if let Some(dir) = self.builtin_runtime().state_dir {
            return Ok(dir);
        }
        Ok(GlobalConfig::data_home_with(&self.env)?.join(DEFAULT_STATE_SUBDIR))
    }

    /// Effective base image tag for setup/teardown containers (repo > global > None).
    pub fn base_image(&self) -> Option<String> {
        if let Some(v) = self.repo.base_image.as_deref() {
            return Some(v.to_string());
        }
        self.global.base_image.clone()
    }

    /// Effective cap on concurrently-running workflow steps
    /// (flags > env > repo > global > `None` = unlimited).
    pub fn effective_max_concurrent_agents(&self) -> Option<usize> {
        if let Some(n) = self.flags.max_concurrent_agents {
            return Some(n);
        }
        if let Some(n) = self.env.max_concurrent_agents() {
            return Some(n);
        }
        if let Some(n) = self.repo.max_concurrent_agents {
            return Some(n);
        }
        self.global.max_concurrent_agents
    }

    /// Effective credential injection mode (repo > built-in default `keychain`).
    ///
    /// `passthrough` skips keychain injection entirely; `none` disables keychain
    /// injection; `keychain` (default) injects host keychain creds, subject to
    /// injection-time dedup of any repo-declared env vars.
    ///
    /// Single-layer resolution (repo only, no global flag override) is a
    /// deliberate conservative choice: a credential toggle must be an explicit
    /// per-repo decision, not silently inherited from a global default that the
    /// operator may not realize is set.
    pub fn auth_mode(&self) -> AgentAuthMode {
        self.repo.auth.unwrap_or_default()
    }

    /// Effective live credential-refresh configuration (repo > global >
    /// built-in). `enabled: false` is the escape hatch that restores legacy
    /// env-variable credential delivery.
    pub fn auth_refresh(&self) -> AuthRefreshSettings {
        let repo = self.repo.auth_refresh.as_ref();
        let global = self.global.auth_refresh.as_ref();
        AuthRefreshSettings {
            enabled: repo
                .and_then(|c| c.enabled)
                .or_else(|| global.and_then(|c| c.enabled))
                .unwrap_or(true),
            threshold: Duration::from_secs(
                repo.and_then(|c| c.threshold_minutes)
                    .or_else(|| global.and_then(|c| c.threshold_minutes))
                    .unwrap_or(20)
                    .saturating_mul(60),
            ),
            tick: Duration::from_secs(
                repo.and_then(|c| c.tick_seconds)
                    .or_else(|| global.and_then(|c| c.tick_seconds))
                    .unwrap_or(60),
            ),
        }
    }

    /// `worktree.onDirtyMergeTarget`: repo over global. `None` when neither
    /// sets it, so the host's headless default applies.
    pub fn worktree_on_dirty_merge_target(&self) -> Option<OnDirtyMergeTarget> {
        self.repo
            .worktree
            .as_ref()
            .and_then(|w| w.on_dirty_merge_target)
            .or_else(|| {
                self.global
                    .worktree
                    .as_ref()
                    .and_then(|w| w.on_dirty_merge_target)
            })
    }

    /// `worktree.mergeFix.onFailure`: repo over global.
    pub fn worktree_merge_fix_on_failure(&self) -> Option<OnMergeFailure> {
        let merge_fix = |w: &Option<WorktreeConfig>| w.as_ref().and_then(|w| w.merge_fix.clone());
        merge_fix(&self.repo.worktree)
            .and_then(|m| m.on_failure)
            .or_else(|| merge_fix(&self.global.worktree).and_then(|m| m.on_failure))
    }

    /// `worktree.mergeFix.promptTemplate`: repo over global. A path relative
    /// to the git root.
    pub fn worktree_merge_fix_prompt_template(&self) -> Option<String> {
        let merge_fix = |w: &Option<WorktreeConfig>| w.as_ref().and_then(|w| w.merge_fix.clone());
        merge_fix(&self.repo.worktree)
            .and_then(|m| m.prompt_template)
            .or_else(|| merge_fix(&self.global.worktree).and_then(|m| m.prompt_template))
    }
}

impl EffectiveConfig {
    pub fn prerequisite_policy(
        &self,
        context: super::workflows::PrerequisiteResolveContext,
    ) -> super::workflows::PrerequisitePolicy {
        use super::workflows::{PrerequisitePolicy, PrerequisitePolicyLayers};
        PrerequisitePolicy::resolve(PrerequisitePolicyLayers {
            global: self
                .global
                .workflows
                .as_ref()
                .and_then(|w| w.prerequisites.as_ref()),
            squad_global: self
                .flags
                .squad_prerequisites
                .then_some(())
                .and(self.global.squad.as_ref())
                .and_then(|s| s.workflows.as_ref())
                .and_then(|w| w.prerequisites.as_ref()),
            repo: self
                .repo
                .workflows
                .as_ref()
                .and_then(|w| w.prerequisites.as_ref()),
            task: self.flags.squad_task_prerequisites.as_ref(),
            flags: &self.flags.prerequisites,
            context,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::env::{
        EnvSnapshot, AWMAN_API_KEY, AWMAN_REMOTE_ADDR, AWMAN_REMOTE_SESSION,
    };
    use crate::data::config::repo::{
        AgentAuthMode, AgentFlagsConfig, ApiConfig, AuthRefreshConfig, RemoteConfig,
    };
    use std::time::Duration;

    fn make_effective(
        flags: FlagConfig,
        env: EnvSnapshot,
        repo: RepoConfig,
        global: GlobalConfig,
    ) -> EffectiveConfig {
        EffectiveConfig::new(flags, env, repo, global)
    }

    // ─── agent ────────────────────────────────────────────────────────────────

    #[test]
    fn agent_flag_beats_repo_and_global() {
        let flags = FlagConfig {
            agent: Some("flag-agent".to_string()),
            ..Default::default()
        };
        let repo = RepoConfig {
            agent: Some("repo-agent".to_string()),
            ..Default::default()
        };
        let global = GlobalConfig {
            default_agent: Some("global-agent".to_string()),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.agent().as_deref(), Some("flag-agent"));
    }

    #[test]
    fn agent_repo_beats_global() {
        let repo = RepoConfig {
            agent: Some("repo-agent".to_string()),
            ..Default::default()
        };
        let global = GlobalConfig {
            default_agent: Some("global-agent".to_string()),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.agent().as_deref(), Some("repo-agent"));
    }

    #[test]
    fn agent_global_is_used_when_repo_unset() {
        let global = GlobalConfig {
            default_agent: Some("global-agent".to_string()),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.agent().as_deref(), Some("global-agent"));
    }

    #[test]
    fn agent_none_when_all_unset() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.agent(), None);
    }

    // ─── scrollback_lines ─────────────────────────────────────────────────────

    #[test]
    fn scrollback_flag_beats_repo_and_global() {
        let flags = FlagConfig {
            terminal_scrollback_lines: Some(9999),
            ..Default::default()
        };
        let repo = RepoConfig {
            terminal_scrollback_lines: Some(5000),
            ..Default::default()
        };
        let global = GlobalConfig {
            terminal_scrollback_lines: Some(2000),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.scrollback_lines(), 9999);
    }

    #[test]
    fn scrollback_repo_beats_global() {
        let repo = RepoConfig {
            terminal_scrollback_lines: Some(5000),
            ..Default::default()
        };
        let global = GlobalConfig {
            terminal_scrollback_lines: Some(2000),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.scrollback_lines(), 5000);
    }

    #[test]
    fn scrollback_global_beats_built_in_default() {
        let global = GlobalConfig {
            terminal_scrollback_lines: Some(3333),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.scrollback_lines(), 3333);
    }

    #[test]
    fn scrollback_built_in_default_is_10000() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.scrollback_lines(), DEFAULT_SCROLLBACK_LINES);
        assert_eq!(ec.scrollback_lines(), 10_000);
    }

    // ─── agent_stuck_timeout ──────────────────────────────────────────────────

    #[test]
    fn timeout_flag_beats_repo_and_global() {
        let flags = FlagConfig {
            agent_stuck_timeout: Some(Duration::from_secs(999)),
            ..Default::default()
        };
        let repo = RepoConfig {
            agent_stuck_timeout_secs: Some(100),
            ..Default::default()
        };
        let global = GlobalConfig {
            agent_stuck_timeout_secs: Some(50),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(999));
    }

    #[test]
    fn timeout_repo_beats_global() {
        let repo = RepoConfig {
            agent_stuck_timeout_secs: Some(77),
            ..Default::default()
        };
        let global = GlobalConfig {
            agent_stuck_timeout_secs: Some(50),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(77));
    }

    #[test]
    fn timeout_global_beats_built_in_default() {
        let global = GlobalConfig {
            agent_stuck_timeout_secs: Some(120),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(120));
    }

    #[test]
    fn timeout_built_in_default_is_30s() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(
            ec.agent_stuck_timeout(),
            Duration::from_secs(DEFAULT_AGENT_STUCK_TIMEOUT_SECS)
        );
        assert_eq!(ec.agent_stuck_timeout(), Duration::from_secs(30));
    }

    // ─── yolo_disallowed_tools ────────────────────────────────────────────────

    #[test]
    fn yolo_disallowed_tools_flag_beats_repo() {
        let flags = FlagConfig {
            yolo_disallowed_tools: Some(vec!["flag-tool".to_string()]),
            ..Default::default()
        };
        let repo = RepoConfig {
            yolo_disallowed_tools: Some(vec!["repo-tool".to_string()]),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, GlobalConfig::default());
        assert_eq!(ec.yolo_disallowed_tools(), vec!["flag-tool"]);
    }

    #[test]
    fn yolo_disallowed_tools_repo_beats_global() {
        let repo = RepoConfig {
            yolo_disallowed_tools: Some(vec!["repo-tool".to_string()]),
            ..Default::default()
        };
        let global = GlobalConfig {
            yolo_disallowed_tools: Some(vec!["global-tool".to_string()]),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.yolo_disallowed_tools(), vec!["repo-tool"]);
    }

    #[test]
    fn yolo_disallowed_tools_empty_when_all_unset() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert!(ec.yolo_disallowed_tools().is_empty());
    }

    // ─── agent_flags ──────────────────────────────────────────────────────────

    fn agent_flags_map(entries: &[(&str, &[&str])]) -> Option<AgentFlagsConfig> {
        Some(
            entries
                .iter()
                .map(|(agent, values)| {
                    (
                        agent.to_string(),
                        values.iter().map(|v| v.to_string()).collect(),
                    )
                })
                .collect(),
        )
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn agent_flags_empty_when_all_unset() {
        let ec = EffectiveConfig::default();
        assert!(ec.agent_flags("codex").is_empty());
    }

    #[test]
    fn agent_flags_global_entry_used_when_repo_has_none() {
        let global = GlobalConfig {
            agent_flags: agent_flags_map(&[("codex", &["--no-daemon"])]),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.agent_flags("codex"), strings(&["--no-daemon"]));
        assert!(ec.agent_flags("claude").is_empty());
    }

    #[test]
    fn agent_flags_repo_entry_replaces_global_entry_per_agent() {
        let repo = RepoConfig {
            agent_flags: agent_flags_map(&[
                ("codex", &["--no-daemon", "--config", "x=y"]),
                ("claude", &[]),
            ]),
            ..Default::default()
        };
        let global = GlobalConfig {
            agent_flags: agent_flags_map(&[
                ("codex", &["--no-daemon"]),
                ("claude", &["--verbose"]),
                ("gemini", &["--debug"]),
            ]),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(
            ec.agent_flags("codex"),
            strings(&["--no-daemon", "--config", "x=y"])
        );
        // An empty repo entry turns the global one off.
        assert!(ec.agent_flags("claude").is_empty());
        // Agents the repo does not mention keep their global entry.
        assert_eq!(ec.agent_flags("gemini"), strings(&["--debug"]));
    }

    #[test]
    fn agent_flags_cli_replaces_config_for_the_selected_agent_only() {
        let flags = FlagConfig {
            agent: Some("codex".into()),
            agent_flags: Some(strings(&["--search"])),
            ..Default::default()
        };
        let repo = RepoConfig {
            agent_flags: agent_flags_map(&[
                ("codex", &["--no-daemon"]),
                ("claude", &["--verbose"]),
            ]),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, GlobalConfig::default());
        assert_eq!(ec.agent_flags("codex"), strings(&["--search"]));
        // A different agent (e.g. a workflow step override) keeps its config.
        assert_eq!(ec.agent_flags("claude"), strings(&["--verbose"]));
    }

    #[test]
    fn agent_flags_cli_targets_the_config_selected_agent_without_agent_flag() {
        let flags = FlagConfig {
            agent_flags: Some(strings(&["--search"])),
            ..Default::default()
        };
        let repo = RepoConfig {
            agent: Some("codex".into()),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, GlobalConfig::default());
        assert_eq!(ec.agent_flags("codex"), strings(&["--search"]));
        assert!(ec.agent_flags("claude").is_empty());
    }

    // ─── remote_default_addr ──────────────────────────────────────────────────

    #[test]
    fn remote_addr_flag_beats_env_and_global() {
        let flags = FlagConfig {
            remote_addr: Some("flag-addr".to_string()),
            ..Default::default()
        };
        let env = EnvSnapshot::with_overrides([(AWMAN_REMOTE_ADDR, "env-addr")]);
        let global = GlobalConfig {
            remote: Some(RemoteConfig {
                default_addr: Some("global-addr".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(flags, env, RepoConfig::default(), global);
        assert_eq!(ec.remote_default_addr().as_deref(), Some("flag-addr"));
    }

    #[test]
    fn remote_addr_env_beats_global() {
        let env = EnvSnapshot::with_overrides([(AWMAN_REMOTE_ADDR, "env-addr")]);
        let global = GlobalConfig {
            remote: Some(RemoteConfig {
                default_addr: Some("global-addr".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), env, RepoConfig::default(), global);
        assert_eq!(ec.remote_default_addr().as_deref(), Some("env-addr"));
    }

    #[test]
    fn remote_addr_global_is_used_when_flag_and_env_unset() {
        let global = GlobalConfig {
            remote: Some(RemoteConfig {
                default_addr: Some("global-addr".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.remote_default_addr().as_deref(), Some("global-addr"));
    }

    #[test]
    fn remote_addr_none_when_all_unset() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.remote_default_addr(), None);
    }

    // ─── remote_default_api_key ───────────────────────────────────────────────

    #[test]
    fn remote_api_key_flag_beats_env() {
        let flags = FlagConfig {
            api_key: Some("flag-key".to_string()),
            ..Default::default()
        };
        let env = EnvSnapshot::with_overrides([(AWMAN_API_KEY, "env-key")]);
        let ec = make_effective(flags, env, RepoConfig::default(), GlobalConfig::default());
        assert_eq!(ec.remote_default_api_key().as_deref(), Some("flag-key"));
    }

    #[test]
    fn remote_api_key_env_beats_global() {
        let env = EnvSnapshot::with_overrides([(AWMAN_API_KEY, "env-key")]);
        let global = GlobalConfig {
            remote: Some(RemoteConfig {
                default_api_key: Some("global-key".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), env, RepoConfig::default(), global);
        assert_eq!(ec.remote_default_api_key().as_deref(), Some("env-key"));
    }

    // ─── remote_session ───────────────────────────────────────────────────────

    #[test]
    fn remote_session_flag_beats_env() {
        let flags = FlagConfig {
            remote_session: Some("flag-session".to_string()),
            ..Default::default()
        };
        let env = EnvSnapshot::with_overrides([(AWMAN_REMOTE_SESSION, "env-session")]);
        let ec = make_effective(flags, env, RepoConfig::default(), GlobalConfig::default());
        assert_eq!(ec.remote_session().as_deref(), Some("flag-session"));
    }

    #[test]
    fn remote_session_from_env_when_flag_unset() {
        let env = EnvSnapshot::with_overrides([(AWMAN_REMOTE_SESSION, "env-session")]);
        let ec = make_effective(
            FlagConfig::default(),
            env,
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.remote_session().as_deref(), Some("env-session"));
    }

    #[test]
    fn remote_session_none_when_both_unset() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.remote_session(), None);
    }

    // ─── always_non_interactive ───────────────────────────────────────────────

    #[test]
    fn always_non_interactive_flag_wins() {
        let flags = FlagConfig {
            non_interactive: Some(true),
            ..Default::default()
        };
        let global = GlobalConfig {
            api: Some(ApiConfig {
                always_non_interactive: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), RepoConfig::default(), global);
        assert!(ec.always_non_interactive());
    }

    #[test]
    fn always_non_interactive_from_global_when_flag_unset() {
        let global = GlobalConfig {
            api: Some(ApiConfig {
                always_non_interactive: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert!(ec.always_non_interactive());
    }

    #[test]
    fn always_non_interactive_default_is_false() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert!(!ec.always_non_interactive());
    }

    // ─── api_work_dirs ───────────────────────────────────────────────────

    #[test]
    fn api_work_dirs_from_global() {
        let global = GlobalConfig {
            api: Some(ApiConfig {
                work_dirs: Some(vec!["/data".to_string(), "/work".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.api_work_dirs(), vec!["/data", "/work"]);
    }

    #[test]
    fn api_work_dirs_empty_when_not_set() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert!(ec.api_work_dirs().is_empty());
    }

    // ─── api_work_dir_roots ──────────────────────────────────────────────

    #[test]
    fn api_work_dir_roots_from_global() {
        let global = GlobalConfig {
            api: Some(ApiConfig {
                work_dir_roots: Some(vec!["/srv/repos".to_string(), "/more".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.api_work_dir_roots(), vec!["/srv/repos", "/more"]);
        assert!(
            ec.api_work_dirs().is_empty(),
            "roots do not leak into workdirs"
        );
    }

    #[test]
    fn api_work_dir_roots_empty_when_not_set() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert!(ec.api_work_dir_roots().is_empty());
    }

    // ─── runtime ─────────────────────────────────────────────────────────────

    #[test]
    fn runtime_from_global() {
        let global = GlobalConfig {
            runtime: Some("podman".to_string()),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.runtime().as_deref(), Some("podman"));
    }

    #[test]
    fn runtime_none_when_not_set() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.runtime(), None);
    }

    // ─── full-stack precedence tests ─────────────────────────────────────────

    #[test]
    fn full_stack_agent_precedence_flag_beats_repo_beats_global_beats_none() {
        let flags = FlagConfig {
            agent: Some("flag-agent".to_string()),
            ..Default::default()
        };
        let repo = RepoConfig {
            agent: Some("repo-agent".to_string()),
            ..Default::default()
        };
        let global = GlobalConfig {
            default_agent: Some("global-agent".to_string()),
            ..Default::default()
        };

        // Flag wins over all.
        let ec = make_effective(
            flags.clone(),
            EnvSnapshot::empty(),
            repo.clone(),
            global.clone(),
        );
        assert_eq!(
            ec.agent().as_deref(),
            Some("flag-agent"),
            "flag should beat repo and global"
        );

        // Remove flag → repo wins.
        let ec2 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo.clone(),
            global.clone(),
        );
        assert_eq!(
            ec2.agent().as_deref(),
            Some("repo-agent"),
            "repo should beat global"
        );

        // Remove repo → global wins.
        let ec3 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(
            ec3.agent().as_deref(),
            Some("global-agent"),
            "global used when flag and repo absent"
        );

        // Remove all → None.
        let ec4 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec4.agent(), None, "None when nothing is set");
    }

    #[test]
    fn full_stack_flag_wins_over_all_levels_for_scrollback() {
        // Set scrollback at every level; flag must win.
        let flags = FlagConfig {
            terminal_scrollback_lines: Some(1111),
            ..Default::default()
        };
        let repo = RepoConfig {
            terminal_scrollback_lines: Some(2222),
            ..Default::default()
        };
        let global = GlobalConfig {
            terminal_scrollback_lines: Some(3333),
            ..Default::default()
        };
        let ec = make_effective(flags, EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.scrollback_lines(), 1111);

        // Remove flag — repo wins.
        let flags2 = FlagConfig::default();
        let repo2 = RepoConfig {
            terminal_scrollback_lines: Some(2222),
            ..Default::default()
        };
        let global2 = GlobalConfig {
            terminal_scrollback_lines: Some(3333),
            ..Default::default()
        };
        let ec2 = make_effective(flags2, EnvSnapshot::empty(), repo2, global2);
        assert_eq!(ec2.scrollback_lines(), 2222);

        // Remove repo — global wins.
        let ec3 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig {
                terminal_scrollback_lines: Some(3333),
                ..Default::default()
            },
        );
        assert_eq!(ec3.scrollback_lines(), 3333);

        // Remove global — built-in default wins.
        let ec4 = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec4.scrollback_lines(), DEFAULT_SCROLLBACK_LINES);
    }

    // ─── effective_max_concurrent_agents (WI-0096) ──────────────────────────

    fn env_with_max_concurrent(n: &str) -> EnvSnapshot {
        EnvSnapshot::with_overrides([(crate::data::config::env::AWMAN_MAX_CONCURRENT_AGENTS, n)])
    }

    #[test]
    fn max_concurrent_flag_beats_env_repo_and_global() {
        let flags = FlagConfig {
            max_concurrent_agents: Some(1),
            ..Default::default()
        };
        let repo = RepoConfig {
            max_concurrent_agents: Some(3),
            ..Default::default()
        };
        let global = GlobalConfig {
            max_concurrent_agents: Some(4),
            ..Default::default()
        };
        let ec = make_effective(flags, env_with_max_concurrent("2"), repo, global);
        assert_eq!(ec.effective_max_concurrent_agents(), Some(1));
    }

    #[test]
    fn max_concurrent_env_beats_repo_and_global() {
        let repo = RepoConfig {
            max_concurrent_agents: Some(3),
            ..Default::default()
        };
        let global = GlobalConfig {
            max_concurrent_agents: Some(4),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            env_with_max_concurrent("2"),
            repo,
            global,
        );
        assert_eq!(ec.effective_max_concurrent_agents(), Some(2));
    }

    #[test]
    fn max_concurrent_repo_beats_global() {
        let repo = RepoConfig {
            max_concurrent_agents: Some(3),
            ..Default::default()
        };
        let global = GlobalConfig {
            max_concurrent_agents: Some(4),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert_eq!(ec.effective_max_concurrent_agents(), Some(3));
    }

    #[test]
    fn max_concurrent_global_used_when_repo_env_flag_unset() {
        let global = GlobalConfig {
            max_concurrent_agents: Some(4),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.effective_max_concurrent_agents(), Some(4));
    }

    #[test]
    fn max_concurrent_none_when_all_unset_means_unlimited() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.effective_max_concurrent_agents(), None);
    }

    // ── Part B: auth_mode ─────────────────────────────────────────────────────

    #[test]
    fn auth_mode_default_is_keychain() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(
            ec.auth_mode(),
            AgentAuthMode::Keychain,
            "default auth mode must be keychain"
        );
    }

    #[test]
    fn auth_mode_passthrough_from_repo() {
        let repo = RepoConfig {
            auth: Some(AgentAuthMode::Passthrough),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo,
            GlobalConfig::default(),
        );
        assert_eq!(
            ec.auth_mode(),
            AgentAuthMode::Passthrough,
            "passthrough set in repo config must be effective"
        );
    }

    #[test]
    fn auth_mode_none_from_repo() {
        let repo = RepoConfig {
            auth: Some(AgentAuthMode::None),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo,
            GlobalConfig::default(),
        );
        assert_eq!(
            ec.auth_mode(),
            AgentAuthMode::None,
            "none set in repo config must be effective"
        );
    }

    #[test]
    fn auth_mode_keychain_explicitly_set_in_repo() {
        let repo = RepoConfig {
            auth: Some(AgentAuthMode::Keychain),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo,
            GlobalConfig::default(),
        );
        assert_eq!(ec.auth_mode(), AgentAuthMode::Keychain);
    }

    // ── launch_mode ──────────────────────────────────────────────────────────

    #[test]
    fn launch_mode_defaults_to_stdio_and_ignores_global_config() {
        let global = GlobalConfig {
            launch_mode_fallback: Some(LaunchModeFallback::Stdio),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.launch_mode(), LaunchMode::Stdio);
    }

    #[test]
    fn launch_mode_uses_repo_when_flag_and_env_are_unset() {
        let repo = RepoConfig {
            launch_mode: Some(LaunchMode::Acp),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo,
            GlobalConfig::default(),
        );
        assert_eq!(ec.launch_mode(), LaunchMode::Acp);
    }

    #[test]
    fn launch_mode_env_beats_repo() {
        let env =
            EnvSnapshot::with_overrides([(crate::data::config::env::AWMAN_LAUNCH_MODE, "stdio")]);
        let repo = RepoConfig {
            launch_mode: Some(LaunchMode::Acp),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), env, repo, GlobalConfig::default());
        assert_eq!(ec.launch_mode(), LaunchMode::Stdio);
    }

    #[test]
    fn launch_mode_flag_beats_env_and_repo() {
        let flags = FlagConfig {
            launch_mode: Some(LaunchMode::Acp),
            ..Default::default()
        };
        let env =
            EnvSnapshot::with_overrides([(crate::data::config::env::AWMAN_LAUNCH_MODE, "stdio")]);
        let repo = RepoConfig {
            launch_mode: Some(LaunchMode::Stdio),
            ..Default::default()
        };
        let ec = make_effective(flags, env, repo, GlobalConfig::default());
        assert_eq!(ec.launch_mode(), LaunchMode::Acp);
    }

    // ── launch_mode_fallback ─────────────────────────────────────────────────

    #[test]
    fn launch_mode_fallback_defaults_to_error() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(ec.launch_mode_fallback(), LaunchModeFallback::Error);
    }

    #[test]
    fn launch_mode_fallback_uses_global_config_only() {
        let global = GlobalConfig {
            launch_mode_fallback: Some(LaunchModeFallback::Stdio),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert_eq!(ec.launch_mode_fallback(), LaunchModeFallback::Stdio);
    }

    // ── worktree merge-back settings (WI-0132 Part B) ────────────────────────

    #[test]
    fn worktree_on_dirty_merge_target_repo_overrides_global() {
        use crate::data::config::repo::{OnDirtyMergeTarget, WorktreeConfig};
        let worktree = |v| {
            Some(WorktreeConfig {
                on_dirty_merge_target: Some(v),
                merge_fix: None,
            })
        };
        let unset = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(unset.worktree_on_dirty_merge_target(), None);

        let global_only = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig {
                worktree: worktree(OnDirtyMergeTarget::Stash),
                ..Default::default()
            },
        );
        assert_eq!(
            global_only.worktree_on_dirty_merge_target(),
            Some(OnDirtyMergeTarget::Stash)
        );

        let both = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig {
                worktree: worktree(OnDirtyMergeTarget::Commit),
                ..Default::default()
            },
            GlobalConfig {
                worktree: worktree(OnDirtyMergeTarget::Stash),
                ..Default::default()
            },
        );
        assert_eq!(
            both.worktree_on_dirty_merge_target(),
            Some(OnDirtyMergeTarget::Commit)
        );
    }

    // ── auth_refresh (WI-0107 §6) ────────────────────────────────────────────

    #[test]
    fn auth_refresh_defaults_when_unset_anywhere() {
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(
            ec.auth_refresh(),
            AuthRefreshSettings {
                enabled: true,
                threshold: Duration::from_secs(20 * 60),
                tick: Duration::from_secs(60),
            }
        );
    }

    #[test]
    fn auth_refresh_repo_overrides_global_per_field() {
        let repo = RepoConfig {
            auth_refresh: Some(AuthRefreshConfig {
                enabled: None,
                threshold_minutes: Some(5),
                tick_seconds: None,
            }),
            ..Default::default()
        };
        let global = GlobalConfig {
            auth_refresh: Some(AuthRefreshConfig {
                enabled: Some(false),
                threshold_minutes: Some(30),
                tick_seconds: Some(15),
            }),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        let settings = ec.auth_refresh();
        // repo sets threshold_minutes explicitly, so it wins over global's.
        assert_eq!(
            settings.threshold,
            Duration::from_secs(5 * 60),
            "repo threshold_minutes must override global"
        );
        // repo leaves `enabled` and `tick_seconds` unset, so each falls
        // through to global independently (per-field precedence, not
        // per-object).
        assert!(
            !settings.enabled,
            "repo left `enabled` unset; must inherit global's false"
        );
        assert_eq!(
            settings.tick,
            Duration::from_secs(15),
            "repo left tick_seconds unset; must inherit global's value"
        );
    }

    #[test]
    fn auth_refresh_repo_kill_switch_overrides_global_enabled_true() {
        let repo = RepoConfig {
            auth_refresh: Some(AuthRefreshConfig {
                enabled: Some(false),
                threshold_minutes: None,
                tick_seconds: None,
            }),
            ..Default::default()
        };
        let global = GlobalConfig {
            auth_refresh: Some(AuthRefreshConfig {
                enabled: Some(true),
                threshold_minutes: None,
                tick_seconds: None,
            }),
            ..Default::default()
        };
        let ec = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert!(
            !ec.auth_refresh().enabled,
            "repo-level kill switch (enabled: false) must win over an enabled global default"
        );
    }

    #[test]
    fn auth_refresh_global_only_is_used_when_repo_unset() {
        let global = GlobalConfig {
            auth_refresh: Some(AuthRefreshConfig {
                enabled: Some(true),
                threshold_minutes: Some(45),
                tick_seconds: Some(90),
            }),
            ..Default::default()
        };
        let ec = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        let settings = ec.auth_refresh();
        assert!(settings.enabled);
        assert_eq!(settings.threshold, Duration::from_secs(45 * 60));
        assert_eq!(settings.tick, Duration::from_secs(90));
    }

    #[test]
    fn builtin_runtime_merges_repo_over_global() {
        let global = GlobalConfig {
            builtin: Some(BuiltinRuntimeConfig {
                vcpus: Some(2),
                memory_mib: Some(8192),
                ..Default::default()
            }),
            ..Default::default()
        };
        let repo = RepoConfig {
            builtin: Some(BuiltinRuntimeConfig {
                vcpus: Some(4),
                ..Default::default()
            }),
            ..Default::default()
        };
        let eff = make_effective(FlagConfig::default(), EnvSnapshot::default(), repo, global);
        let merged = eff.builtin_runtime();
        assert_eq!(merged.vcpus, Some(4));
        assert_eq!(merged.memory_mib, Some(8192));
        assert_eq!(
            EffectiveConfig::default().builtin_runtime(),
            BuiltinRuntimeConfig::default()
        );
    }

    #[test]
    fn builtin_settings_resolve_defaults_and_process_values() {
        let resolved = EffectiveConfig::default().builtin_settings().unwrap();
        assert_eq!(
            resolved.launch.vcpus,
            crate::data::config::builtin_runtime::DEFAULT_VCPUS
        );
        assert_eq!(
            resolved.launch.memory_mib,
            crate::data::config::builtin_runtime::DEFAULT_MEMORY_MIB
        );
        assert_eq!(resolved.launch.network, BuiltinNetworkSettings::default());
        assert!(!resolved.builder_enabled);
        assert!(resolved.image_source.is_none());
        assert!(resolved.images.is_empty());
        assert!(resolved.registries.is_empty());
        assert!(!resolved.state_dir.as_os_str().is_empty());
    }

    #[test]
    fn builtin_settings_apply_network_narrowing_and_reject_widening() {
        use crate::data::config::builtin_network::{BuiltinNetworkConfig, NetworkMode};
        let global = GlobalConfig {
            builtin: Some(BuiltinRuntimeConfig {
                network: Some(BuiltinNetworkConfig {
                    mode: Some(NetworkMode::Allowlist),
                    allow: Some(vec!["*.example.com".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let narrowed = RepoConfig {
            builtin: Some(BuiltinRuntimeConfig {
                network: Some(BuiltinNetworkConfig {
                    mode: Some(NetworkMode::Allowlist),
                    allow: Some(vec!["api.example.com".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            narrowed,
            global.clone(),
        )
        .builtin_settings()
        .unwrap();
        assert_eq!(resolved.launch.network.allow.len(), 1);
        assert_eq!(resolved.launch.network.allow[0].name(), "api.example.com");

        let widening = RepoConfig {
            builtin: Some(BuiltinRuntimeConfig {
                network: Some(BuiltinNetworkConfig {
                    mode: Some(NetworkMode::Public),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            make_effective(
                FlagConfig::default(),
                EnvSnapshot::empty(),
                widening,
                global
            )
            .builtin_settings(),
            Err(DataError::BuiltinConfig(_))
        ));

        let invalid_resources = RepoConfig {
            builtin: Some(BuiltinRuntimeConfig {
                vcpus: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            make_effective(
                FlagConfig::default(),
                EnvSnapshot::empty(),
                invalid_resources,
                GlobalConfig::default(),
            )
            .builtin_settings(),
            Err(DataError::BuiltinConfig(_))
        ));
    }

    #[test]
    fn builtin_settings_return_registry_builder_and_normalized_legacy_sources() {
        use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
        let global: BuiltinRuntimeConfig = serde_json::from_str(
            r#"{
                "imageSource":{"type":"docker-store","host":"unix:///tmp/docker.sock"},
                "images":{"agent:latest":{"type":"apple-store"}},
                "registries":{"registry.example":{"insecure":true}},
                "builder":{"enabled":true}
            }"#,
        )
        .unwrap();
        let resolved = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            GlobalConfig {
                builtin: Some(global),
                ..Default::default()
            },
        )
        .builtin_settings()
        .unwrap();

        assert!(resolved.builder_enabled);
        assert!(resolved.registries.contains_key("registry.example"));
        assert!(matches!(
            resolved.image_source,
            Some(ImageSourceSpec::DockerStore { .. })
        ));
        assert_eq!(
            resolved.image_source.as_ref().unwrap().kind(),
            ImageSourceKind::DockerStore
        );
        assert_eq!(
            resolved.images.get("agent:latest").unwrap().kind(),
            ImageSourceKind::AppleStore
        );
    }

    #[test]
    fn builtin_settings_preserve_session_state_dir_for_runtime_check() {
        let repo = RepoConfig {
            builtin: Some(BuiltinRuntimeConfig {
                state_dir: Some("/repo/builtin-state".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            repo,
            GlobalConfig::default(),
        )
        .builtin_settings()
        .unwrap();
        assert_eq!(
            resolved.launch.repo_state_dir,
            Some("/repo/builtin-state".into())
        );
        assert_eq!(resolved.state_dir, resolved.launch.repo_state_dir.unwrap());
    }

    #[test]
    fn builtin_builder_enabled_inherits_and_repo_can_disable_global() {
        use crate::data::config::builtin_runtime::BuiltinBuilderConfig;
        let global = GlobalConfig {
            builtin: Some(BuiltinRuntimeConfig {
                builder: Some(BuiltinBuilderConfig {
                    enabled: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let inherited = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global.clone(),
        );
        assert!(inherited.builtin_builder_enabled());
        let repo = RepoConfig {
            builtin: Some(BuiltinRuntimeConfig {
                builder: Some(BuiltinBuilderConfig {
                    enabled: Some(false),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let disabled = make_effective(FlagConfig::default(), EnvSnapshot::empty(), repo, global);
        assert!(!disabled.builtin_builder_enabled());
        assert!(!EffectiveConfig::default().builtin_builder_enabled());
    }

    #[test]
    fn builder_enablement_does_not_select_a_non_builtin_runtime() {
        use crate::data::config::builtin_runtime::BuiltinBuilderConfig;
        let global = GlobalConfig {
            runtime: Some("docker".into()),
            builtin: Some(BuiltinRuntimeConfig {
                builder: Some(BuiltinBuilderConfig {
                    enabled: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let effective = make_effective(
            FlagConfig::default(),
            EnvSnapshot::empty(),
            RepoConfig::default(),
            global,
        );
        assert!(effective.builtin_builder_enabled());
        assert_eq!(effective.runtime_selection(), Ok(RuntimeSelection::Docker));
    }

    #[test]
    fn builtin_state_dir_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_str().unwrap().to_string();
        let env = EnvSnapshot::with_overrides([(
            crate::data::config::env::AWMAN_CONFIG_HOME,
            home.as_str(),
        )]);
        let eff = make_effective(
            FlagConfig::default(),
            env.clone(),
            RepoConfig::default(),
            GlobalConfig::default(),
        );
        assert_eq!(
            eff.builtin_state_dir().unwrap(),
            tmp.path().join(DEFAULT_STATE_SUBDIR)
        );

        let global = GlobalConfig {
            builtin: Some(BuiltinRuntimeConfig {
                state_dir: Some("/cfg-state".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let eff = make_effective(
            FlagConfig::default(),
            env,
            RepoConfig::default(),
            global.clone(),
        );
        assert_eq!(
            eff.builtin_state_dir().unwrap(),
            PathBuf::from("/cfg-state")
        );

        let env = EnvSnapshot::with_overrides([
            (crate::data::config::env::AWMAN_CONFIG_HOME, home.as_str()),
            (
                crate::data::config::env::AWMAN_BUILTIN_STATE_DIR,
                "/env-state",
            ),
        ]);
        let eff = make_effective(FlagConfig::default(), env, RepoConfig::default(), global);
        assert_eq!(
            eff.builtin_state_dir().unwrap(),
            PathBuf::from("/env-state")
        );
    }

    fn builtin_layers(global: &str, repo: Option<&str>, env: EnvSnapshot) -> EffectiveConfig {
        make_effective(
            FlagConfig::default(),
            env,
            RepoConfig {
                builtin: repo.map(|r| serde_json::from_str(r).unwrap()),
                ..Default::default()
            },
            GlobalConfig {
                builtin: Some(serde_json::from_str(global).unwrap()),
                ..Default::default()
            },
        )
    }

    #[test]
    fn builtin_runtime_view_has_no_network_in_either_layer_shape() {
        let global_only = builtin_layers(
            r#"{"network":{"mode":"none"},"vcpus":3}"#,
            None,
            EnvSnapshot::empty(),
        );
        assert_eq!(global_only.builtin_runtime().network, None);
        assert_eq!(global_only.builtin_runtime().vcpus, Some(3));
        let layered = builtin_layers(
            r#"{"network":{"mode":"allowlist","allow":["*.example.com"]}}"#,
            Some(r#"{"network":{"allow":["api.example.com"]}}"#),
            EnvSnapshot::empty(),
        );
        assert_eq!(layered.builtin_runtime().network, None);
        assert_eq!(layered.builtin_network().unwrap().allow.len(), 1);
    }

    /// `BuiltinRuntimeConfig::validate` no longer checks the network block:
    /// `builtin_network` rejects a self-contradictory block in either layer,
    /// naming the layer, which covers everything the merged-block check did.
    #[test]
    fn builtin_network_rejects_a_contradictory_block_in_either_layer() {
        let contradictory = r#"{"network":{"mode":"none","hostPorts":[8765]}}"#;
        let global = builtin_layers(contradictory, None, EnvSnapshot::empty());
        let error = global.builtin_network().unwrap_err();
        assert!(error.starts_with("global builtin.network: "), "{error}");
        assert!(matches!(
            global.builtin_launch_settings(),
            Err(DataError::BuiltinConfig(_))
        ));
        let repo = builtin_layers("{}", Some(contradictory), EnvSnapshot::empty());
        let error = repo.builtin_network().unwrap_err();
        assert!(error.starts_with("repo builtin.network: "), "{error}");
        // A repo block that only restates the global mode is not narrowing
        // anything, but is consistent and accepted.
        let shadowing = builtin_layers(
            r#"{"network":{"mode":"none"}}"#,
            Some(r#"{"network":{"mode":"none"}}"#),
            EnvSnapshot::empty(),
        );
        assert!(shadowing.builtin_network().is_ok());
    }

    #[test]
    fn builtin_launch_settings_default_without_any_builtin_block() {
        assert_eq!(
            EffectiveConfig::default()
                .builtin_launch_settings()
                .unwrap(),
            BuiltinLaunchSettings::default()
        );
    }

    #[test]
    fn builtin_launch_settings_take_repo_resources_and_narrowed_network() {
        let settings = builtin_layers(
            r#"{"vcpus":4,"memoryMib":8192,"rootDiskMib":8192,"mountQuotaMib":1024,"network":{"hostPorts":[8765]}}"#,
            Some(r#"{"vcpus":1,"mountQuotaMib":512,"network":{"mode":"allowlist","allow":["api.example.com"]}}"#),
            EnvSnapshot::empty(),
        )
        .builtin_launch_settings()
        .unwrap();
        assert_eq!((settings.vcpus, settings.memory_mib), (1, 8192));
        assert_eq!(
            (settings.root_disk_mib, settings.mount_quota_mib),
            (8192, 512)
        );
        assert_eq!(
            settings.network.mode,
            crate::data::config::NetworkMode::Allowlist
        );
        assert_eq!(settings.network.allow.len(), 1);
        assert_eq!(settings.network.host_ports, vec![8765]);
        assert_eq!(settings.repo_state_dir, None);
    }

    #[test]
    fn builtin_launch_settings_reject_widening_and_bad_resources() {
        for (global, repo) in [
            (
                r#"{"network":{"mode":"none"}}"#,
                r#"{"network":{"mode":"public"}}"#,
            ),
            (r#"{}"#, r#"{"network":{"hostPorts":[8765]}}"#),
        ] {
            match builtin_layers(global, Some(repo), EnvSnapshot::empty()).builtin_launch_settings()
            {
                Err(DataError::BuiltinConfig(message)) => {
                    assert!(message.contains("builtin.network"), "{message}")
                }
                other => panic!("a widening repo block must fail closed: {other:?}"),
            }
        }
        for repo in [
            r#"{"vcpus":0}"#,
            r#"{"memoryMib":64}"#,
            r#"{"rootDiskMib":64}"#,
            r#"{"mountQuotaMib":0}"#,
        ] {
            assert!(matches!(
                builtin_layers("{}", Some(repo), EnvSnapshot::empty()).builtin_launch_settings(),
                Err(DataError::BuiltinConfig(_))
            ));
        }
    }

    #[test]
    fn builtin_launch_settings_name_a_repo_state_dir_only_when_the_repo_sets_one() {
        let global_only = builtin_layers(r#"{"stateDir":"/g"}"#, None, EnvSnapshot::empty());
        assert_eq!(
            global_only
                .builtin_launch_settings()
                .unwrap()
                .repo_state_dir,
            None
        );
        let repo = builtin_layers(
            r#"{"stateDir":"/g"}"#,
            Some(r#"{"stateDir":"/r"}"#),
            EnvSnapshot::empty(),
        );
        assert_eq!(
            repo.builtin_launch_settings().unwrap().repo_state_dir,
            Some(PathBuf::from("/r"))
        );
        // The environment variable outranks every config layer, so the repo
        // value resolves to the same directory the process uses.
        let env = EnvSnapshot::with_overrides([(
            crate::data::config::env::AWMAN_BUILTIN_STATE_DIR,
            "/env-state",
        )]);
        let overridden = builtin_layers("{}", Some(r#"{"stateDir":"/r"}"#), env);
        assert_eq!(
            overridden.builtin_launch_settings().unwrap().repo_state_dir,
            Some(PathBuf::from("/env-state"))
        );
    }

    #[test]
    fn runtime_selection_is_global_only_and_defaults_to_docker() {
        assert_eq!(
            EffectiveConfig::default().runtime_selection(),
            Ok(RuntimeSelection::Docker)
        );
    }
}

#[cfg(test)]
mod squad_prerequisite_review_tests {
    use super::*;
    #[test]
    fn squad_defaults_apply_without_a_stored_task_policy_and_only_to_squad_runs() {
        let global: GlobalConfig = serde_json::from_value(serde_json::json!({"squad":{"workflows":{"prerequisites":{"ssh":{"use":"deny"},"unresolved":"warn"}}}})).unwrap();
        for squad in [true, false] {
            let config = EffectiveConfig::new(
                FlagConfig {
                    squad_prerequisites: squad,
                    ..Default::default()
                },
                EnvSnapshot::default(),
                RepoConfig::default(),
                global.clone(),
            );
            let policy = config.prerequisite_policy(Default::default());
            assert_eq!(
                policy.ssh.use_ == crate::data::config::workflows::CredentialUse::Deny,
                squad
            );
        }
    }
}
