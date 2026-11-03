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
use crate::data::config::overlays::{ContextOverlaySpec, ContextScope, TypedOverlay};
use crate::data::config::repo::LaunchMode;
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
        flags.launch_mode = launch_mode_flag;
        let config = EffectiveConfig::new(
            flags,
            current.env().clone(),
            current.repo().clone(),
            current.global().clone(),
        );
        let repo = current.repo();
        let explicit_acp = launch_mode_flag == Some(LaunchMode::Acp)
            || (agent_flag.is_none()
                && repo.agent.is_some()
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
        use crate::data::config::global::LaunchModeFallback;
        use crate::data::config::repo::LaunchMode;

        if config.launch_mode() == LaunchMode::Stdio {
            return Ok(LaunchModeDecision::Stdio);
        }
        if AgentRegistry::resolve_for_session(session, agent)?.supports_acp {
            return Ok(LaunchModeDecision::Acp);
        }
        if explicit_single_agent_acp || config.launch_mode_fallback() == LaunchModeFallback::Error {
            return Err(EngineError::AcpUnsupported {
                agent: agent.as_str().to_string(),
            });
        }
        Ok(LaunchModeDecision::StdioWithFallbackWarning)
    }

    pub fn acp_fallback_warning(agent: &AgentName) -> String {
        format!(
            "agent '{}' does not support ACP; falling back to stdio for this session — see launchModeFallback",
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
        if specs.is_empty() {
            return Ok((vec![], None));
        }

        let resolver = ContextDirResolver::from_process_env()
            .map_err(|e| CommandError::Other(format!("context dir resolver: {e}")))?;

        // Read once, not once per spec: `remote_url` shells out to git.
        let remote_url = self.remote_url();

        let mut overlays = Vec::new();
        let mut builder = ContextPromptBuilder::new();
        let mut saw_repo_fallback: Option<std::path::PathBuf> = None;

        for spec in specs {
            let (host_path, container_path) = match spec.scope {
                ContextScope::Global => {
                    let p = resolver.global_dir();
                    (p, std::path::PathBuf::from("/awman/context/global"))
                }
                ContextScope::Repo => {
                    let p = resolver.repo_dir(remote_url.as_deref(), self.session.git_root());
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

            if let Err(e) = ContextDirResolver::ensure_exists(&host_path) {
                sink.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!(
                        "context overlay: failed to create directory {}: {e}",
                        host_path.display()
                    ),
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
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "context(repo): no git remote configured; using local fallback directory {}",
                    path.display()
                ),
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

    /// Emit launch-time configuration warnings: the once-per-repo
    /// `dynamicWorkflows` deprecation notice (gated by `notices`), deprecated
    /// `envPassthrough` fields and `agentFlags` entries awman already manages.
    pub fn warn_config(
        &self,
        notices: &crate::command::config_notices::ConfigNotices,
        sink: &mut dyn UserMessageSink,
    ) {
        notices.warn_legacy_dynamic_workflows(self.session, sink);
        let ec = self.session.effective_config();
        if ec.repo().legacy_env_passthrough.is_some() {
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "'.awman/config.json' contains a deprecated 'envPassthrough' field. Move these vars to the 'overlays' array as env() expressions, e.g. \"env(VAR_NAME)\", then remove 'envPassthrough'.".into(),
            });
        }
        if ec.global().legacy_env_passthrough.is_some() {
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "'~/.awman/config.json' contains a deprecated 'envPassthrough' field. Move these vars to the 'overlays' array as env() expressions, e.g. \"env(VAR_NAME)\", then remove 'envPassthrough'.".into(),
            });
        }
        // Load-time warnings (an unknown `workflows.prerequisites` key, a repo
        // `allowApiClientGrants`, …): surfaced, never fatal (WI 0149 §5).
        for (file, warnings) in [
            ("'~/.awman/config.json'", &ec.global().load_warnings),
            ("'.awman/config.json'", &ec.repo().load_warnings),
        ] {
            for warning in warnings {
                sink.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: format!("{file}: {warning}"),
                });
            }
        }
        self.warn_redundant_agent_flags(&ec, sink);
    }

    /// Warn once per agent and flag when `agentFlags` (or `--agent-flag`)
    /// names a flag awman already passes on that agent's interactive
    /// launches (codex `--no-daemon`). The engine drops the duplicate on
    /// those launches; a non-interactive launch keeps it as configured, and
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
                             non-interactive runs may reject it. Remove it from agentFlags."
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

#[cfg(test)]
mod launch_mode_tests {
    use super::*;
    use crate::data::config::global::{GlobalConfig, LaunchModeFallback};
    use crate::data::config::repo::LaunchMode;

    fn config(fallback: LaunchModeFallback) -> EffectiveConfig {
        EffectiveConfig::new(
            crate::data::config::FlagConfig {
                launch_mode: Some(LaunchMode::Acp),
                ..Default::default()
            },
            Default::default(),
            Default::default(),
            GlobalConfig {
                launch_mode_fallback: Some(fallback),
                ..Default::default()
            },
        )
    }

    #[test]
    fn repo_acp_policy_uses_session_env_and_repo_precedence() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("global");
        let session = Session::for_tests_isolated(root.path(), &home);
        let dir = root.path().join(".awman/agents");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pi.toml");
        let source =
            "schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[image]\ninstall=['install pi']\n";
        let name = AgentName::new("pi").unwrap();
        let approve = || {
            let untrusted =
                AgentRegistry::load_untrusted(session.env(), &name, Some(root.path())).unwrap();
            AgentRegistry::record_approval(
                session.env(),
                untrusted.definition(),
                Some(root.path()),
            )
            .unwrap();
        };
        std::fs::write(&path, format!("{source}[acp]\ncommand=['pi','--acp']\n")).unwrap();
        approve();
        assert_eq!(
            LaunchPolicy::resolve_launch_mode(
                &session,
                &config(LaunchModeFallback::Stdio),
                &name,
                false
            )
            .unwrap(),
            LaunchModeDecision::Acp
        );
        std::fs::create_dir_all(home.join("agents")).unwrap();
        std::fs::write(
            home.join("agents/pi.toml"),
            format!("{source}[acp]\ncommand=['pi','--acp']\n"),
        )
        .unwrap();
        std::fs::write(&path, source).unwrap();
        approve();
        assert_eq!(
            LaunchPolicy::resolve_launch_mode(
                &session,
                &config(LaunchModeFallback::Stdio),
                &name,
                false
            )
            .unwrap(),
            LaunchModeDecision::StdioWithFallbackWarning
        );
    }

    #[test]
    fn fallback_decision_matrix() {
        let root = tempfile::tempdir().unwrap();
        let session = Session::for_tests(root.path());
        let supported = AgentName::new("cline").unwrap();
        let unsupported = AgentName::new("claude").unwrap();

        assert_eq!(
            LaunchPolicy::resolve_launch_mode(
                &session,
                &config(LaunchModeFallback::Error),
                &supported,
                false
            )
            .unwrap(),
            LaunchModeDecision::Acp
        );
        assert!(matches!(
            LaunchPolicy::resolve_launch_mode(
                &session,
                &config(LaunchModeFallback::Error),
                &unsupported,
                false
            ),
            Err(EngineError::AcpUnsupported { .. })
        ));
        assert_eq!(
            LaunchPolicy::resolve_launch_mode(
                &session,
                &config(LaunchModeFallback::Stdio),
                &unsupported,
                false
            )
            .unwrap(),
            LaunchModeDecision::StdioWithFallbackWarning
        );
        assert!(matches!(
            LaunchPolicy::resolve_launch_mode(
                &session,
                &config(LaunchModeFallback::Stdio),
                &unsupported,
                true
            ),
            Err(EngineError::AcpUnsupported { .. })
        ));
    }
}

#[cfg(test)]
mod warn_config_tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use crate::data::message::{MessageLevel, RecordingMessageSink};
    use crate::data::session::Session;

    fn open_session(git_root: &std::path::Path, env: EnvSnapshot) -> Session {
        Session::for_tests_with_env(git_root, env)
    }

    #[test]
    fn repo_legacy_env_passthrough_triggers_warning_mentioning_config_path() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();

        // Write a repo config with the legacy envPassthrough field.
        let awman_dir = git_tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"envPassthrough": ["MY_VAR"]}"#,
        )
        .unwrap();

        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let mut sink = RecordingMessageSink::new();
        LaunchPolicy::for_session(&session).warn_config(&Default::default(), &mut sink);

        let warnings: Vec<_> = sink
            .queued()
            .iter()
            .filter(|m| m.level == MessageLevel::Warning)
            .collect();
        assert!(
            !warnings.is_empty(),
            "must emit at least one warning for legacy envPassthrough in repo config"
        );
        let text = &warnings[0].text;
        assert!(
            text.contains(".awman/config.json"),
            "warning must mention .awman/config.json to identify the source file; got: {text}"
        );
    }

    /// An `agentFlags` entry awman already manages warns once, naming the
    /// agent and the flag; an unrelated entry does not.
    #[test]
    fn an_agent_flag_awman_already_passes_triggers_one_warning() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();
        let awman_dir = git_tmp.path().join(".awman");
        std::fs::create_dir_all(&awman_dir).unwrap();
        std::fs::write(
            awman_dir.join("config.json"),
            r#"{"agentFlags": {"codex": ["--no-daemon", "--search"], "claude": ["--verbose"]}}"#,
        )
        .unwrap();
        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let mut sink = RecordingMessageSink::new();
        LaunchPolicy::for_session(&session).warn_config(&Default::default(), &mut sink);
        let warnings: Vec<_> = sink
            .queued()
            .iter()
            .filter(|m| m.level == MessageLevel::Warning)
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].text.contains("codex"), "{}", warnings[0].text);
        assert!(
            warnings[0].text.contains("--no-daemon"),
            "{}",
            warnings[0].text
        );
    }

    #[test]
    fn global_legacy_env_passthrough_triggers_warning_mentioning_global_config_path() {
        let git_tmp = tempfile::tempdir().unwrap();
        let cfg_tmp = tempfile::tempdir().unwrap();

        // Write a global config with the legacy envPassthrough field.
        std::fs::write(
            cfg_tmp.path().join("config.json"),
            r#"{"envPassthrough": ["GLOBAL_VAR"]}"#,
        )
        .unwrap();

        let env =
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, cfg_tmp.path().to_str().unwrap())]);
        let session = open_session(git_tmp.path(), env);

        let mut sink = RecordingMessageSink::new();
        LaunchPolicy::for_session(&session).warn_config(&Default::default(), &mut sink);

        let warnings: Vec<_> = sink
            .queued()
            .iter()
            .filter(|m| m.level == MessageLevel::Warning)
            .collect();
        assert!(
            !warnings.is_empty(),
            "must emit at least one warning for legacy envPassthrough in global config"
        );
        let text = &warnings[0].text;
        assert!(
            text.contains("~/.awman/config.json"),
            "warning must mention ~/.awman/config.json to identify the source file; got: {text}"
        );
    }

    #[test]
    fn no_legacy_fields_produces_no_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
        let session = open_session(tmp.path(), env);

        let mut sink = RecordingMessageSink::new();
        LaunchPolicy::for_session(&session).warn_config(&Default::default(), &mut sink);

        assert!(
            sink.queued().is_empty(),
            "no warning must be emitted when no legacy fields are present; got {:?}",
            sink.queued()
        );
    }
}

#[cfg(test)]
mod resolve_agent_tests {
    use super::LaunchPolicy;
    use crate::data::session::{Session, SessionOpenOptions};

    fn session_with_default_agent(agent: Option<&str>) -> Session {
        let tmp = tempfile::tempdir().expect("scratch git root");
        let session = Session::for_tests_with_options(
            tmp.path(),
            SessionOpenOptions {
                flags: crate::data::config::FlagConfig {
                    agent: agent.map(str::to_string),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        std::mem::forget(tmp);
        session
    }

    #[test]
    fn an_explicit_flag_wins() {
        let session = session_with_default_agent(Some("claude"));
        let resolved = LaunchPolicy::for_session(&session)
            .resolve_agent(&Some("codex".to_string()))
            .expect("resolve");
        assert_eq!(resolved.as_str(), "codex");
    }

    /// Moved here from the TUI (WI 0114 F-15): the overlay title used to
    /// re-run this precedence in Layer 3, so the rule was asserted against a
    /// frontend helper rather than against the resolver every caller shares.
    #[test]
    fn the_session_default_is_used_when_no_flag_is_given() {
        let session = session_with_default_agent(Some("codex"));
        let resolved = LaunchPolicy::for_session(&session)
            .resolve_agent(&None)
            .expect("resolve");
        assert_eq!(
            resolved.as_str(),
            "codex",
            "a configured default agent must beat the hard-coded fallback"
        );
    }

    #[test]
    fn claude_is_the_fallback_without_config() {
        let session = session_with_default_agent(None);
        let resolved = LaunchPolicy::for_session(&session)
            .resolve_agent(&None)
            .expect("resolve");
        assert_eq!(resolved.as_str(), "claude");
    }
}

#[cfg(test)]
mod cross_entrypoint_tests {
    use super::*;
    use crate::command::commands::agents::test_support::{FakeFrontend, FakeRuntime};
    use crate::command::commands::chat::{ChatCommand, ChatCommandFlags, ChatCommandFrontend};
    use crate::command::commands::exec_prompt::{
        ExecPromptCommand, ExecPromptCommandFlags, ExecPromptCommandFrontend,
    };
    use crate::command::commands::Command;
    use crate::command::dispatch::Engines;
    use crate::data::session::AgentHandle;
    use crate::engine::agent_runtime::{
        AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ReadyAgentOptions,
        ResolvedAgentOptions,
    };
    use std::sync::{Arc, Mutex};

    impl ChatCommandFrontend for FakeFrontend {}
    impl ExecPromptCommandFrontend for FakeFrontend {}
    impl crate::engine::acp::AcpFrontend for FakeFrontend {
        fn render_update(&mut self, _: crate::engine::acp::protocol::SessionUpdate) {
            unreachable!("tests stop at runtime build")
        }
        fn request_permission(
            &mut self,
            _: crate::engine::acp::protocol::PermissionRequest,
        ) -> crate::engine::acp::protocol::PermissionDecision {
            unreachable!("tests stop at runtime build")
        }
        fn next_prompt(&mut self) -> Option<String> {
            unreachable!("tests stop at runtime build")
        }
    }

    /// Capture the final options at the real command/runtime boundary. No
    /// process is launched, even when the selected transport is ACP.
    struct CaptureLaunch {
        fake: FakeRuntime,
        options: Mutex<Vec<ResolvedAgentOptions>>,
    }
    impl AgentRuntimeEngine for CaptureLaunch {
        fn runtime_name(&self) -> &'static str {
            "capture"
        }
        fn display_name(&self) -> &'static str {
            "Capture"
        }
        fn capabilities(&self) -> &Capabilities {
            &self.fake.caps
        }
        fn is_available(&self) -> bool {
            true
        }
        fn build(
            &self,
            options: ResolvedAgentOptions,
        ) -> Result<Box<dyn AgentInstance>, EngineError> {
            self.options.lock().unwrap().push(options);
            Err(EngineError::Other("captured launch".into()))
        }
        fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
            unreachable!()
        }
        fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
            unreachable!()
        }
        fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
            unreachable!()
        }
        fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
            unreachable!()
        }
        fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
            unreachable!()
        }
        fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
            unreachable!()
        }
        fn ready_agent(
            &self,
            _: &str,
            _: ReadyAgentOptions,
            _: &mut dyn UserMessageSink,
        ) -> Result<(), EngineError> {
            unreachable!()
        }
        fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
            Ok(true)
        }
        fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
            Ok(Some("/root".into()))
        }
        fn build_image(
            &self,
            _: &crate::engine::container::BuildRequest,
            _: &mut dyn FnMut(&str),
        ) -> Result<(), EngineError> {
            unreachable!()
        }
    }

    #[derive(Default)]
    struct LaunchCase {
        repo_agent: Option<&'static str>,
        repo_mode: Option<LaunchMode>,
        agent: Option<&'static str>,
        mode: Option<LaunchMode>,
        overlay: Vec<String>,
        plan: bool,
        yolo: bool,
        non_interactive: bool,
    }

    async fn run_case(
        case: LaunchCase,
    ) -> (Vec<ResolvedAgentOptions>, Vec<CommandError>, Vec<String>) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".awman")).unwrap();
        std::fs::write(
            tmp.path().join(".awman/config.json"),
            serde_json::to_vec(&serde_json::json!({
                "agent": case.repo_agent,
                "launchMode": case.repo_mode,
                "auth": "none",
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join("home")).unwrap();
        std::fs::write(
            tmp.path().join("home/config.json"),
            r#"{"launchModeFallback":"stdio"}"#,
        )
        .unwrap();
        let session = Session::for_tests_isolated(tmp.path(), &tmp.path().join("home"));
        let capture = Arc::new(CaptureLaunch {
            fake: FakeRuntime::new(),
            options: Default::default(),
        });
        let mut engines = Engines::for_tests(tmp.path());
        engines.runtime = capture.clone();
        engines.agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            engines.overlay_engine.clone(),
            capture.clone(),
        ));
        let mut errors = Vec::new();
        let mut messages = Vec::new();
        let (frontend, log) = FakeFrontend::new();
        errors.push(
            ChatCommand::new(
                ChatCommandFlags {
                    non_interactive: case.non_interactive,
                    plan: case.plan,
                    allow_docker: false,
                    yolo: case.yolo,
                    auto: false,
                    agent: case.agent.map(str::to_string),
                    model: Some("test-model".into()),
                    launch_mode: case.mode,
                    overlay: case.overlay.clone(),
                    agent_flags: None,
                },
                engines.clone(),
                session.clone(),
            )
            .run_with_frontend(Box::new(frontend))
            .await
            .unwrap_err(),
        );
        messages.extend(log.lock().unwrap().messages.iter().map(|m| m.text.clone()));
        let (frontend, log) = FakeFrontend::new();
        errors.push(
            ExecPromptCommand::new(
                ExecPromptCommandFlags {
                    prompt: Some("seeded test prompt".into()),
                    non_interactive: case.non_interactive,
                    plan: case.plan,
                    allow_docker: false,
                    yolo: case.yolo,
                    auto: false,
                    agent: case.agent.map(str::to_string),
                    model: Some("test-model".into()),
                    launch_mode: case.mode,
                    overlay: case.overlay,
                    agent_flags: None,
                    issue_source: Default::default(),
                },
                engines,
                session,
            )
            .run_with_frontend(Box::new(frontend))
            .await
            .unwrap_err(),
        );
        messages.extend(log.lock().unwrap().messages.iter().map(|m| m.text.clone()));
        let options = std::mem::take(&mut *capture.options.lock().unwrap());
        (options, errors, messages)
    }

    #[tokio::test]
    async fn explicit_and_repo_paired_acp_refuse_before_setup_in_both_commands() {
        for case in [
            LaunchCase {
                mode: Some(LaunchMode::Acp),
                ..Default::default()
            },
            LaunchCase {
                repo_agent: Some("claude"),
                repo_mode: Some(LaunchMode::Acp),
                ..Default::default()
            },
        ] {
            let (options, errors, _) = run_case(case).await;
            assert!(options.is_empty());
            assert!(
                errors.iter().all(|error| matches!(error,
                    CommandError::Engine(EngineError::AcpUnsupported { agent }) if agent == "claude"
                )),
                "{errors:?}"
            );
        }
    }

    #[tokio::test]
    async fn repo_acp_fallback_and_explicit_stdio_match_across_commands() {
        for case in [
            LaunchCase {
                repo_mode: Some(LaunchMode::Acp),
                ..Default::default()
            },
            LaunchCase {
                repo_agent: Some("claude"),
                repo_mode: Some(LaunchMode::Acp),
                agent: Some("claude"),
                ..Default::default()
            },
            LaunchCase {
                repo_agent: Some("claude"),
                repo_mode: Some(LaunchMode::Acp),
                mode: Some(LaunchMode::Stdio),
                ..Default::default()
            },
        ] {
            let expect_warning = case.mode.is_none();
            let (options, errors, messages) = run_case(case).await;
            assert_eq!(options.len(), 2, "{errors:?}");
            for option in options {
                let ResolvedAgentOptions::Container(option) = option else {
                    panic!("expected container options")
                };
                assert!(!option.acp);
            }
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m.contains("falling back to stdio"))
                    .count(),
                if expect_warning { 2 } else { 0 }
            );
        }
    }

    #[tokio::test]
    async fn supported_acp_and_plan_yolo_prompt_options_match_across_commands() {
        for (mode, plan) in [
            (LaunchMode::Stdio, true),
            (LaunchMode::Stdio, false),
            (LaunchMode::Acp, true),
            (LaunchMode::Acp, false),
        ] {
            let (options, errors, _) = run_case(LaunchCase {
                agent: Some("cline"),
                mode: Some(mode),
                plan,
                yolo: !plan,
                non_interactive: true,
                overlay: vec![
                    "env(F08_FIRST),env(F08_SECOND)".into(),
                    "env(F08_THIRD)".into(),
                ],
                ..Default::default()
            })
            .await;
            assert_eq!(options.len(), 2, "{errors:?}");
            for (index, option) in options.iter().enumerate() {
                let ResolvedAgentOptions::Container(option) = option else {
                    panic!("expected container options")
                };
                assert_eq!(option.acp, mode == LaunchMode::Acp);
                assert!(!option.interactive);
                assert_eq!(
                    option.plan,
                    if plan {
                        crate::engine::container::options::PlanMode::Enabled
                    } else {
                        crate::engine::container::options::PlanMode::Disabled
                    }
                );
                assert_eq!(
                    option.yolo,
                    if plan {
                        crate::engine::container::options::YoloMode::Disabled
                    } else {
                        crate::engine::container::options::YoloMode::Enabled
                    }
                );
                assert_eq!(
                    option.seeded_prompt.as_deref(),
                    if index == 0 || mode == LaunchMode::Acp {
                        None
                    } else {
                        Some("seeded test prompt")
                    }
                );
                let names: Vec<_> = option
                    .env_passthrough
                    .iter()
                    .map(|name| name.0.as_str())
                    .collect();
                assert!(names
                    .windows(3)
                    .any(|names| names == ["F08_FIRST", "F08_SECOND", "F08_THIRD"]));
            }
        }
    }

    #[tokio::test]
    async fn plan_and_yolo_conflict_before_build_in_both_commands() {
        let (options, errors, _) = run_case(LaunchCase {
            plan: true,
            yolo: true,
            ..Default::default()
        })
        .await;
        assert!(options.is_empty());
        assert!(errors.iter().all(|error| matches!(error,
            CommandError::Engine(EngineError::ConflictingOptions(message)) if message.contains("plan and yolo")
        )), "{errors:?}");
    }

    #[tokio::test]
    async fn malformed_repeated_overlay_retains_the_failing_flag_in_both_commands() {
        let bad = "context(unknown)";
        let (options, errors, _) = run_case(LaunchCase {
            overlay: vec!["env(F08_FIRST)".into(), bad.into()],
            ..Default::default()
        })
        .await;
        assert!(options.is_empty());
        assert!(errors.iter().all(|error| matches!(error,
            CommandError::InvalidOverlaySpec { spec, reason } if spec == bad && !reason.is_empty()
        )), "{errors:?}");
    }
}
