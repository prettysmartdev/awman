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
    /// `if agent.as_str() == "gemini"` arm with a duplicated copy of the
    /// text; they now ask here.
    pub fn deprecation_warning(agent: &AgentName) -> Option<UserMessage> {
        first_party(agent.as_str())?
            .deprecation_note
            .map(|text| UserMessage {
                level: MessageLevel::Warning,
                text,
            })
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
