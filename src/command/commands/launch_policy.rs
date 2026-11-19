//! Layer 2 — `LaunchPolicy`: the decisions a command makes when it is about
//! to put an agent in front of a user.
//!
//! Which agent runs, whether it runs over ACP or stdio, which context
//! directories it gets, and how the end of the session is reported are all
//! business logic, so they live here rather than beside the Layer 0 overlay
//! grammar they consume (`data::config::overlays`, WI 0114 F-27).
//!
//! Stateless helpers parse command overlay flags and report launch outcomes;
//! session-bound policy resolves agent and transport intent against config.

pub mod agent_session;

pub use crate::command::commands::agent_trust::{
    ensure_agent_trusted, ensure_workflow_agents_trusted, AgentTrustFrontend, AgentTrustReview,
    HostCommandReview, TrustDecision, TrustOutcome,
};
use crate::command::error::CommandError;
use crate::data::config::agents::LaunchMode;
use crate::data::config::overlays::{ContextOverlaySpec, ContextScope, TypedOverlay};
use crate::data::config::EffectiveConfig;
use crate::data::fs::ContextDirResolver;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session};
use crate::engine::agent::definition::SystemPromptMode;
use crate::engine::agent::first_party::{first_party, FIRST_PARTY_AGENTS};
use crate::engine::agent::registry::AgentRegistry;
use crate::engine::agent_runtime::execution::AgentExitInfo;
use crate::engine::context_prompt::{ContextPromptBuilder, WorkflowStepInfo};
use crate::engine::error::EngineError;
use crate::engine::git::GitEngine;
use crate::engine::overlay::ContextOverlay;

/// Result of resolving an ACP request for a concrete agent.
///
/// This is deliberately command-layer policy: the engine remains the final
/// safety guard, while this decision determines whether a repository default
/// may use the configured fallback before any setup, overlay, or runtime work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchModeDecision {
    Stdio,
    Acp,
    StdioWithFallbackWarning,
}

/// The launch-time policy for one session.
///
/// Borrows the session rather than owning it: every caller already holds one,
/// and the policy keeps no state of its own.
pub struct LaunchPolicy<'a> {
    session: &'a Session,
    git: Option<&'a GitEngine>,
}

impl<'a> LaunchPolicy<'a> {
    pub fn for_session(session: &'a Session) -> Self {
        Self { session, git: None }
    }

    /// Supply the git engine `resolve_context_overlays` needs to name the
    /// repository's `context(repo)` directory.
    ///
    /// Layer 0's `ContextDirResolver` no longer shells out to git (WI 0114
    /// F-29), so the `origin` URL is read here and passed down. Without a git
    /// engine the resolver falls back to the `_local/{dirname}` directory —
    /// the same directory a repository with no remote has always used.
    pub fn with_git(mut self, git: &'a GitEngine) -> Self {
        self.git = Some(git);
        self
    }

    /// The repository's `origin` URL, or `None` when there is no git engine,
    /// no remote, or the remote is configured empty.
    fn remote_url(&self) -> Option<String> {
        self.git
            .and_then(|git| git.remote_effective_url(self.session.git_root(), "origin"))
    }

    /// Resolve the agent name to use for a command, in precedence order:
    ///   1. explicit CLI flag (`flag`)
    ///   2. `session.default_agent()` (which itself resolves flag > repo > global)
    ///   3. fallback to `"claude"` so a fresh repo with no config still works.
    ///
    /// Frontends (CLI / TUI / API session-setup) must funnel through this helper
    /// so the agent choice is uniformly driven by `.awman/config.json`, with the
    /// hard-coded fallback only used as a last resort.
    pub fn resolve_agent(&self, flag: &Option<String>) -> Result<AgentName, CommandError> {
        if let Some(name) = flag.as_deref() {
            return AgentName::new(name).map_err(CommandError::from);
        }
        Ok(self.session.configured_agent())
    }

    /// Resolve chat / exec prompt intent against the session's config. A repo
    /// agent paired with repo ACP is explicit only while that agent is selected.
    pub fn single_agent_launch_mode(
        &self,
        agent: &AgentName,
        agent_flag: &Option<String>,
        model_flag: &Option<String>,
        launch_mode_flag: Option<LaunchMode>,
    ) -> Result<LaunchModeDecision, EngineError> {
        let current = self.session.effective_config();
        let mut flags = current.flags().clone();
        flags.agent = agent_flag.clone();
        flags.model = model_flag.clone();
        let mut layers = current.layers().clone();
        if let Some(mode) = launch_mode_flag {
            layers.set.0.push((
                "agents.launchMode".into(),
                serde_json::to_value(mode).expect("launch mode serializes"),
            ));
        }
        let config = EffectiveConfig::resolve(layers, flags, current.env().clone())?;
        let repo = current.layers().repo.agents.clone().unwrap_or_default();
        let explicit_acp = launch_mode_flag == Some(LaunchMode::Acp)
            || (agent_flag.is_none()
                && repo.default.is_some()
                && repo.launch_mode == Some(LaunchMode::Acp));
        Self::resolve_launch_mode(self.session, &config, agent, explicit_acp)
    }

    /// Parse repeated overlay flags in order, retaining the original flag
    /// value in a structured error. The caller supplies its own error message.
    pub fn parse_overlay_flags(values: &[String]) -> Result<Vec<TypedOverlay>, CommandError> {
        let mut overlays = Vec::new();
        for value in values {
            let parsed = crate::command::commands::parse_overlay_list(value).map_err(|reason| {
                CommandError::InvalidOverlaySpec {
                    spec: value.clone(),
                    reason,
                }
            })?;
            overlays.extend(parsed);
        }
        Ok(overlays)
    }

    /// Apply the ACP support and fallback policy after normal agent resolution.
    /// Explicit single-agent ACP intent is never downgraded.
    pub fn resolve_launch_mode(
        session: &Session,
        config: &EffectiveConfig,
        agent: &AgentName,
        explicit_single_agent_acp: bool,
    ) -> Result<LaunchModeDecision, EngineError> {
        use crate::data::config::agents::AcpFallback;
        use crate::data::config::agents::LaunchMode;

        if config.launch_mode() == LaunchMode::Stdio {
            return Ok(LaunchModeDecision::Stdio);
        }
        if AgentRegistry::resolve_for_session(session, agent)?.supports_acp {
            return Ok(LaunchModeDecision::Acp);
        }
        if explicit_single_agent_acp || config.acp_fallback() == AcpFallback::Error {
            return Err(EngineError::AcpUnsupported {
                agent: agent.as_str().to_string(),
            });
        }
        Ok(LaunchModeDecision::StdioWithFallbackWarning)
    }

    pub fn acp_fallback_warning(agent: &AgentName) -> String {
        format!(
            "agent '{}' does not support ACP; falling back to stdio for this session — see agents.acpFallback",
            agent.as_str()
        )
    }

    /// The vendor-deprecation warning for `agent`, or `None` when its vendor
    /// still supports it.
    ///
    /// The note is a per-agent fact and lives in the first-party table
    /// (`AgentDefinition::deprecation_note`, F-32; custom agents carry none).
    /// `chat` and `exec prompt` each carried their own
    /// vendor-specific warning text; they now ask here.
    pub fn deprecation_warning(agent: &AgentName) -> Option<UserMessage> {
        first_party(agent.as_str())?
            .deprecation_note
            .map(|text| UserMessage {
                level: MessageLevel::Warning,
                text,
            })
    }

    /// The per-launch notes a first-party agent declares for this run, in the
    /// order they are shown: the agent's informational `launch_note`
    /// (opencode-v2's isolation note) and, when the run takes the interactive
    /// model fallback, the interface-change warning (opencode-v2 `mini`).
    ///
    /// One helper for every launch path (`chat`, `exec prompt`, workflow
    /// steps, API sessions that go through them) so the same agent shows the
    /// same notes regardless of runtime. Sandbox runtimes receive them through
    /// the same frontend; the Layer 1 sandbox builder adds only the
    /// sandbox-specific `sandbox_auth_note`, so nothing is shown twice.
    pub fn launch_notices(
        agent: &AgentName,
        run: &crate::engine::agent::AgentRunOptions,
    ) -> Vec<UserMessage> {
        let Some(def) = first_party(agent.as_str()) else {
            return Vec::new();
        };
        let mut notes = Vec::new();
        if let Some(text) = &def.launch_note {
            notes.push(UserMessage {
                level: MessageLevel::Info,
                text: text.clone(),
            });
        }
        if let Some(text) = def.interactive_model_notice(run) {
            notes.push(UserMessage {
                level: MessageLevel::Warning,
                text: text.to_string(),
            });
        }
        // WI 0070: an inherited environment default that would select an
        // incompatible starting mode is not passed (the host is unchanged).
        for name in crate::engine::agent::plan_env_dropped(&def, run) {
            notes.push(UserMessage {
                level: MessageLevel::Warning,
                text: format!("{name} is not passed to a --plan launch"),
            });
        }
        notes
    }

    /// Resolve `ContextOverlaySpec`s into `ContextOverlay`s (host paths ensured-to-exist)
    /// and build the combined system prompt. Used by `chat`, `exec_prompt`, and
    /// `exec_workflow`.
    ///
    /// `workflow_step_info` is `Some` when running inside a multi-step workflow
    /// (provides step progress for the prompt); `None` for chat / exec_prompt.
    ///
    /// `workflow_invocation_id` keys the workflow context directory. Inside a
    /// workflow, pass `WorkflowState::invocation_id` so resumed runs reuse the same
    /// dir. For `chat`/`exec prompt`, pass `None` and the session UUID is used as
    /// a stable per-session fallback.
    ///
    /// `agent` is used to emit `Warning`/`Info` `UserMessage`s when the agent's
    /// system-prompt delivery is degraded (`Replace`) or unsupported.
    pub fn resolve_context_overlays(
        &self,
        specs: &[ContextOverlaySpec],
        agent: &AgentName,
        workflow_invocation_id: Option<uuid::Uuid>,
        workflow_step_info: Option<&WorkflowStepInfo>,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(Vec<ContextOverlay>, Option<String>), CommandError> {
        let mut specs = specs.to_vec();
        if matches!(
            self.session.repo_context(),
            crate::data::session::RepoContextBinding::Staged(_)
        ) && !specs.iter().any(|s| s.scope == ContextScope::Repo)
        {
            specs.push(ContextOverlaySpec {
                key: None,
                scope: ContextScope::Repo,
                permission: crate::data::config::overlays::OverlayPermission::ReadWrite,
            });
        }
        if specs.is_empty() {
            return Ok((vec![], None));
        }

        let resolver = ContextDirResolver::from_env(self.session.env()).map_err(|e| {
            tracing::warn!(%e, "context resolver failed");
            CommandError::Other("context directory is unavailable".into())
        })?;

        // Read once, not once per spec: `remote_url` shells out to git.
        let remote_url = self.remote_url();

        let mut overlays = Vec::new();
        let mut builder = ContextPromptBuilder::new();
        let mut saw_repo_fallback: Option<std::path::PathBuf> = None;

        for spec in &specs {
            let (host_path, container_path) = match spec.scope {
                ContextScope::Global => {
                    let p = resolver.global_dir();
                    (p, std::path::PathBuf::from("/awman/context/global"))
                }
                ContextScope::Repo => {
                    let resolved = crate::command::context::scope::RepoContextScope::new(
                        &resolver,
                        self.session,
                    )
                    .resolve(spec.key.as_ref(), remote_url.as_deref());
                    if let Some(ignored) = resolved.ignored_overlay_key {
                        sink.write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text: format!(
                                "context overlay key {} is overridden by the session binding",
                                ignored.as_str()
                            ),
                        });
                    }
                    let p = resolved.host_dir;
                    // Detect the `_local/...` fallback so we can surface an Info
                    // message naming the resolved dir.
                    if let Some(suffix) = p.iter().rev().nth(1).and_then(|c| c.to_str()) {
                        if suffix == "_local" {
                            saw_repo_fallback = Some(p.clone());
                        }
                    }
                    (p, std::path::PathBuf::from("/awman/context/repo"))
                }
                ContextScope::Workflow => {
                    let uuid =
                        workflow_invocation_id.unwrap_or_else(|| self.session.id().as_uuid());
                    let p = resolver.workflow_dir(uuid);
                    (p, std::path::PathBuf::from("/awman/context/workflow"))
                }
            };

            crate::data::fs::context_dirs::validate_context_path(resolver.awman_home(), &host_path)
                .map_err(|e| {
                    tracing::warn!(%e, "context overlay path refused");
                    CommandError::Other("context overlay directory is unavailable".into())
                })?;
            if let Err(e) = ContextDirResolver::ensure_exists(&host_path) {
                tracing::warn!(%e, "context overlay directory creation failed");
                sink.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: "context overlay: failed to create directory".into(),
                });
                continue;
            }

            overlays.push(ContextOverlay {
                scope: spec.scope,
                host_path,
                container_path,
                permission: spec.permission,
            });

            match spec.scope {
                ContextScope::Global => {
                    builder = builder.with_global();
                }
                ContextScope::Repo => {
                    builder = builder.with_repo();
                }
                ContextScope::Workflow => {
                    if let Some(info) = workflow_step_info {
                        builder = builder.with_workflow(info);
                    } else {
                        sink.write_message(UserMessage {
                            level: MessageLevel::Info,
                            text: "context(workflow) is most useful inside a workflow; \
                                   workflow state will be empty"
                                .to_string(),
                        });
                        builder = builder.with_workflow_oneshot();
                    }
                }
            }
        }

        if let Some(path) = saw_repo_fallback {
            tracing::debug!(path = %path.display(), "context repo uses local fallback");
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "context(repo): no git remote configured; using local fallback at /awman/context/repo".into(),
            });
        }

        // Surface agent-compatibility warnings now that we know context overlays
        // will be active for this run.
        if let Ok(def) = AgentRegistry::resolve_for_session(self.session, agent) {
            match &def.system_prompt_delivery {
                SystemPromptMode::Replace => {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "context overlay: '{}' replaces the default system prompt with the \
                             context instructions; a baseline preamble is prepended but you may \
                             see degraded tool-use guidance.",
                            agent.as_str()
                        ),
                    });
                }
                SystemPromptMode::Unsupported => {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "context() overlay mounted for '{}' but system prompt injection is \
                             not supported for this agent; the agent will not be automatically \
                             notified about the context directory.",
                            agent.as_str()
                        ),
                    });
                }
                _ => {}
            }
        }

        let prompt = builder.build();
        Ok((overlays, prompt))
    }

    /// Emit launch-time configuration warnings: `agents.flags` entries awman
    /// already manages.
    pub fn warn_config(&self, sink: &mut dyn UserMessageSink) {
        let ec = self.session.effective_config();
        self.warn_redundant_agent_flags(&ec, sink);
    }

    /// Warn once per agent and flag when `agentFlags` (or `--agent-flag`)
    /// names a flag awman already passes on that agent's interactive
    /// launches (codex `--no-daemon`). The engine drops the duplicate on
    /// those launches; a headless launch keeps it as configured, and
    /// the agent may reject it there (`codex exec` refuses `--no-daemon`).
    fn warn_redundant_agent_flags(&self, ec: &EffectiveConfig, sink: &mut dyn UserMessageSink) {
        for agent in FIRST_PARTY_AGENTS {
            let Some(def) = first_party(agent) else {
                continue;
            };
            let configured = ec.agent_flags(agent);
            for flag in &def.interactive_only_flags {
                if configured.iter().any(|c| c == flag) {
                    sink.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "agentFlags for {agent} includes `{flag}`, which awman already passes \
                             on interactive {agent} launches; it is not added twice there, and \
                             headless runs may reject it. Remove it from agentFlags."
                        ),
                    });
                }
            }
        }
    }

    /// Report the end of an interactive agent session on the sink, reflecting
    /// how it actually ended: Info for a clean exit, Error with the exit code
    /// when the agent exited non-zero, Error when waiting on the agent itself
    /// failed. Shared by `chat` and `exec prompt` so a failed launch is never
    /// reported as a quiet "Agent session ended".
    pub fn report_session_end(
        sink: &mut dyn UserMessageSink,
        command: &str,
        exit: &Result<AgentExitInfo, EngineError>,
    ) {
        let (level, text) = match exit {
            Ok(info) if info.exit_code == 0 => {
                (MessageLevel::Info, "Agent session ended".to_string())
            }
            Ok(info) => (
                MessageLevel::Error,
                format!("Agent session ended with exit code {}", info.exit_code),
            ),
            Err(e) => (
                MessageLevel::Error,
                format!("{command}: failed to wait for agent exit: {e}"),
            ),
        };
        sink.write_message(UserMessage { level, text });
    }
}

// ─── Native start mode and workspace access (WI 0070) ──────────────────────

/// One awman permission-mode flag as the command received it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModeRequest {
    /// The flag's resolved value (explicit, or a frontend/profile default).
    pub value: bool,
    /// The caller supplied it themselves.
    pub explicit: bool,
}

/// Everything one launch asks for that `resolve_agent_policy` decides.
#[derive(Debug, Clone, Copy)]
pub struct AgentPolicyRequest<'r> {
    pub agent: &'r AgentName,
    pub plan: bool,
    pub yolo: ModeRequest,
    pub auto: ModeRequest,
    /// `--read-only-workspace` ∨ a workflow step's `read_only_workspace`.
    /// A read-only lease engine is added by the resolver.
    pub requested_access: crate::data::workspace::WorkspaceAccess,
    pub launch: LaunchModeDecision,
    pub allow_docker: bool,
    /// Route facts for the native plan probe.
    pub non_interactive: bool,
    pub model_requested: bool,
}

/// The one resolution of a launch's native mode, mount access and transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPolicyOutcome {
    /// Reported verbatim (`flags_applied`, results, messages).
    pub policy: crate::data::agent_policy::ResolvedAgentPolicy,
    /// Effective `--yolo` / `--auto` after an inherited default was
    /// suppressed by `--plan`.
    pub yolo: bool,
    pub auto: bool,
    /// The ACP session mode to select and confirm before the first prompt.
    pub acp_mode_id: Option<String>,
    pub notices: Vec<UserMessage>,
}

/// The plain facts the pure resolver needs: no session, no runtime.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PureRequest<'r> {
    pub agent: &'r str,
    pub plan: bool,
    pub yolo: ModeRequest,
    pub auto: ModeRequest,
    pub requested_access: crate::data::workspace::WorkspaceAccess,
    pub engine_read_only: bool,
    pub runtime_supports_read_only: bool,
    pub runtime_name: &'r str,
    pub allow_docker: bool,
    pub plan_kind: Option<crate::data::agent_policy::NativePlanKind>,
    pub launch: LaunchModeDecision,
}

/// The pure core of [`LaunchPolicy::resolve_agent_policy`]. The native-mode
/// half and the mount half never read each other's inputs: `--plan` adds no
/// mount restriction and `--read-only-workspace` selects no native mode.
pub(crate) fn resolve_agent_policy_pure(
    req: &PureRequest<'_>,
) -> Result<AgentPolicyOutcome, CommandError> {
    use crate::data::agent_policy::{NativeStartMode, ResolvedAgentPolicy};
    use crate::data::workspace::WorkspaceAccess;

    // ── native mode half ──
    for (name, mode) in [("yolo", req.yolo), ("auto", req.auto)] {
        if req.plan && mode.value && mode.explicit {
            return Err(CommandError::MutuallyExclusive {
                command: Vec::new(),
                a: "plan".into(),
                b: name.into(),
            });
        }
    }
    let mut notices = Vec::new();
    let (yolo, auto) = if req.plan {
        for (name, mode) in [("yolo", req.yolo), ("auto", req.auto)] {
            if mode.value {
                notices.push(UserMessage {
                    level: MessageLevel::Info,
                    text: format!(
                        "--plan: the inherited --{name} default is not applied to '{}'",
                        req.agent
                    ),
                });
            }
        }
        (false, false)
    } else {
        (req.yolo.value, req.auto.value)
    };
    let native_start_mode = if !req.plan {
        NativeStartMode::Default
    } else {
        match req.plan_kind {
            Some(kind) => kind.into(),
            None => {
                return Err(CommandError::from(EngineError::PlanModeUnsupported {
                    agent: req.agent.to_string(),
                }))
            }
        }
    };

    // ── mount half ──
    let workspace_access = req
        .requested_access
        .strictest(WorkspaceAccess::from_read_only(req.engine_read_only));
    crate::command::commands::workspace_access::check_workspace_access(
        workspace_access,
        req.allow_docker,
        req.runtime_supports_read_only,
        req.runtime_name,
    )?;

    // ── transport ──
    let launch_mode = if req.launch == LaunchModeDecision::Acp {
        LaunchMode::Acp
    } else {
        LaunchMode::Stdio
    };
    Ok(AgentPolicyOutcome {
        policy: ResolvedAgentPolicy {
            native_start_mode,
            workspace_access,
            launch_mode,
        },
        yolo,
        auto,
        acp_mode_id: None,
        notices,
    })
}

impl LaunchPolicy<'_> {
    /// The only place a launch's native start mode, workspace mount access
    /// and transport are decided (WI 0070). Runs before image preparation,
    /// credentials or mounts. A route that cannot deliver a requested
    /// `--plan` (headless/interactive/ACP/sandbox kit) fails here too, by the
    /// same Layer 1 rule `validate_run` applies again at launch.
    pub fn resolve_agent_policy(
        &self,
        req: AgentPolicyRequest<'_>,
        engine: &crate::engine::agent::AgentEngine,
    ) -> Result<AgentPolicyOutcome, CommandError> {
        use crate::engine::agent::definition::RunParadigm;
        use crate::engine::container::options::PlanMode;
        let runtime = engine.runtime();
        let def = if req.plan {
            Some(AgentRegistry::resolve_for_session(self.session, req.agent)?)
        } else {
            None
        };
        let mut outcome = resolve_agent_policy_pure(&PureRequest {
            agent: req.agent.as_str(),
            plan: req.plan,
            yolo: req.yolo,
            auto: req.auto,
            requested_access: req.requested_access,
            engine_read_only: engine.read_only_workspace(),
            runtime_supports_read_only: runtime.capabilities().read_only_workspace(),
            runtime_name: runtime.display_name(),
            allow_docker: req.allow_docker,
            plan_kind: def
                .as_ref()
                .and_then(|d| d.native_plan.as_ref().map(|p| p.kind)),
            launch: req.launch,
        })?;
        if let Some(def) = &def {
            let probe = crate::engine::agent::AgentRunOptions {
                plan: Some(PlanMode::Enabled),
                non_interactive: req.non_interactive,
                model: req.model_requested.then(String::new),
                launch_mode: outcome.policy.launch_mode,
                ..Default::default()
            };
            let paradigm = if runtime.capabilities().kit_declarative {
                RunParadigm::Sandbox
            } else {
                RunParadigm::Container
            };
            outcome.acp_mode_id = def
                .native_plan_delivery(&probe, paradigm)?
                .and_then(|delivery| delivery.acp_mode_id);
        }
        Ok(outcome)
    }

    /// The `Info` line a launch that requested a native mode shows, or
    /// `None` for the default mode.
    pub fn native_mode_message(
        agent: &AgentName,
        policy: &crate::data::agent_policy::ResolvedAgentPolicy,
        step: Option<&str>,
    ) -> Option<UserMessage> {
        use crate::data::agent_policy::NativeStartMode;
        let mode = match policy.native_start_mode {
            NativeStartMode::Default => return None,
            NativeStartMode::Plan => "plan",
            NativeStartMode::ReadOnly => "read-only",
            NativeStartMode::Custom => "custom",
        };
        let prefix = step
            .map(|name| format!("Step '{name}': starting"))
            .unwrap_or_else(|| "Starting".into());
        let tail = if policy.workspace_access.is_read_only() {
            ""
        } else {
            "; workspace mounts unchanged"
        };
        Some(UserMessage {
            level: MessageLevel::Info,
            text: format!("{prefix} {} in native {mode} mode{tail}.", agent.as_str()),
        })
    }
}

#[cfg(test)]
mod agent_policy_tests {
    //! WI 0070: the pure resolver over all four flag combinations, agents
    //! with and without a native plan, runtimes with and without read-only
    //! mounts. The two halves never read each other's inputs.
    use super::*;
    use crate::data::agent_policy::{NativePlanKind, NativeStartMode};
    use crate::data::workspace::WorkspaceAccess::{ReadOnly, ReadWrite};

    fn req(plan: bool, read_only: bool) -> PureRequest<'static> {
        PureRequest {
            agent: "agent",
            plan,
            yolo: ModeRequest::default(),
            auto: ModeRequest::default(),
            requested_access: crate::data::workspace::WorkspaceAccess::from_read_only(read_only),
            engine_read_only: false,
            runtime_supports_read_only: true,
            runtime_name: "Docker",
            allow_docker: false,
            plan_kind: Some(NativePlanKind::Plan),
            launch: LaunchModeDecision::Stdio,
        }
    }

    #[test]
    fn the_four_combinations_resolve_independently() {
        for (plan, read_only, mode, access) in [
            (false, false, NativeStartMode::Default, ReadWrite),
            (true, false, NativeStartMode::Plan, ReadWrite),
            (false, true, NativeStartMode::Default, ReadOnly),
            (true, true, NativeStartMode::Plan, ReadOnly),
        ] {
            let out = resolve_agent_policy_pure(&req(plan, read_only)).unwrap();
            assert_eq!(
                out.policy.native_start_mode, mode,
                "plan={plan} ro={read_only}"
            );
            assert_eq!(
                out.policy.workspace_access, access,
                "plan={plan} ro={read_only}"
            );
            assert_eq!(out.policy.launch_mode, LaunchMode::Stdio);
        }
    }

    #[test]
    fn a_missing_native_plan_refuses_only_plan() {
        let mut no_plan = req(true, false);
        no_plan.plan_kind = None;
        let err = resolve_agent_policy_pure(&no_plan).unwrap_err();
        assert_eq!(
            err.code(),
            crate::data::agent_failure::NATIVE_CAPABILITY_UNAVAILABLE
        );
        assert_eq!(err.http_status(), 409);
        // Mount-only never needs a plan kind.
        let mut mount_only = req(false, true);
        mount_only.plan_kind = None;
        let out = resolve_agent_policy_pure(&mount_only).unwrap();
        assert_eq!(out.policy.native_start_mode, NativeStartMode::Default);
        assert_eq!(out.policy.workspace_access, ReadOnly);
    }

    #[test]
    fn a_runtime_without_read_only_mounts_refuses_only_the_mount_flag() {
        for plan_kind in [Some(NativePlanKind::ReadOnly), None] {
            let mut plan_only = req(true, false);
            plan_only.runtime_supports_read_only = false;
            plan_only.allow_docker = true;
            plan_only.plan_kind = plan_kind.or(Some(NativePlanKind::Custom));
            let out = resolve_agent_policy_pure(&plan_only).unwrap();
            assert_eq!(out.policy.workspace_access, ReadWrite, "plan adds no mount");
        }
        let mut mount = req(false, true);
        mount.runtime_supports_read_only = false;
        mount.runtime_name = "Docker Sandboxes";
        let err = resolve_agent_policy_pure(&mount).unwrap_err();
        assert_eq!(err.code(), "read_only_workspace_unsupported");
        let mut both = req(true, true);
        both.runtime_supports_read_only = false;
        assert!(resolve_agent_policy_pure(&both).is_err());
        let mut docker = req(false, true);
        docker.allow_docker = true;
        assert!(matches!(
            resolve_agent_policy_pure(&docker).unwrap_err(),
            CommandError::InvalidFlagValue { .. }
        ));
    }

    #[test]
    fn explicit_yolo_or_auto_with_plan_is_refused_and_inherited_ones_are_suppressed() {
        for name in ["yolo", "auto"] {
            let explicit = ModeRequest {
                value: true,
                explicit: true,
            };
            let inherited = ModeRequest {
                value: true,
                explicit: false,
            };
            let mut r = req(true, false);
            if name == "yolo" {
                r.yolo = explicit
            } else {
                r.auto = explicit
            }
            let err = resolve_agent_policy_pure(&r).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{name}");
            assert!(
                matches!(err, CommandError::MutuallyExclusive { .. }),
                "{name}"
            );

            let mut r = req(true, false);
            if name == "yolo" {
                r.yolo = inherited
            } else {
                r.auto = inherited
            }
            let out = resolve_agent_policy_pure(&r).unwrap();
            assert!(!out.yolo && !out.auto, "{name} suppressed");
            assert_eq!(out.notices.len(), 1);
            assert!(out.notices[0].text.contains(&format!("--{name}")));

            // Without --plan nothing is suppressed.
            let mut r = req(false, false);
            if name == "yolo" {
                r.yolo = inherited
            } else {
                r.auto = inherited
            }
            let out = resolve_agent_policy_pure(&r).unwrap();
            assert_eq!((out.yolo, out.auto), (name == "yolo", name == "auto"));
            assert!(out.notices.is_empty());
        }
    }

    #[test]
    fn a_read_only_lease_engine_makes_the_launch_read_only_without_a_plan() {
        let mut lease = req(true, false);
        lease.engine_read_only = true;
        let out = resolve_agent_policy_pure(&lease).unwrap();
        assert_eq!(out.policy.workspace_access, ReadOnly);
        assert_eq!(out.policy.native_start_mode, NativeStartMode::Plan);
        let mut lease_no_plan = req(false, false);
        lease_no_plan.engine_read_only = true;
        let out = resolve_agent_policy_pure(&lease_no_plan).unwrap();
        assert_eq!(out.policy.native_start_mode, NativeStartMode::Default);
    }

    #[test]
    fn the_transport_follows_the_launch_decision_and_kinds_map_to_start_modes() {
        let mut acp = req(false, false);
        acp.launch = LaunchModeDecision::Acp;
        assert_eq!(
            resolve_agent_policy_pure(&acp).unwrap().policy.launch_mode,
            LaunchMode::Acp
        );
        let mut fallback = req(false, false);
        fallback.launch = LaunchModeDecision::StdioWithFallbackWarning;
        assert_eq!(
            resolve_agent_policy_pure(&fallback)
                .unwrap()
                .policy
                .launch_mode,
            LaunchMode::Stdio
        );
        for kind in NativePlanKind::ALL {
            let mut r = req(true, false);
            r.plan_kind = Some(kind);
            assert_eq!(
                resolve_agent_policy_pure(&r)
                    .unwrap()
                    .policy
                    .native_start_mode,
                NativeStartMode::from(kind)
            );
        }
    }

    #[test]
    fn the_native_mode_message_says_whether_mounts_changed() {
        let agent = AgentName::new("codex").unwrap();
        let mut policy = crate::data::agent_policy::ResolvedAgentPolicy {
            native_start_mode: NativeStartMode::ReadOnly,
            ..Default::default()
        };
        assert_eq!(
            LaunchPolicy::native_mode_message(&agent, &policy, None)
                .unwrap()
                .text,
            "Starting codex in native read-only mode; workspace mounts unchanged."
        );
        policy.workspace_access = ReadOnly;
        assert_eq!(
            LaunchPolicy::native_mode_message(&agent, &policy, Some("plan"))
                .unwrap()
                .text,
            "Step 'plan': starting codex in native read-only mode."
        );
        policy.native_start_mode = NativeStartMode::Default;
        assert!(LaunchPolicy::native_mode_message(&agent, &policy, None).is_none());
    }
}

pub mod settings_profile;
