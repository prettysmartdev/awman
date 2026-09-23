//! Layer 2 — `LaunchPolicy`: the decisions a command makes when it is about
//! to put an agent in front of a user.
//!
//! Which agent runs, whether it runs over ACP or stdio, which context
//! directories it gets, and how the end of the session is reported are all
//! business logic, so they live here rather than beside the Layer 0 overlay
//! grammar they consume (`data::config::overlays`, WI 0114 F-27).
//!
//! Three of the six entry points are associated functions rather than
//! methods: `resolve_launch_mode` is handed the *command's* effective config
//! (flags merged in), not the session's, and `acp_fallback_warning` and
//! `report_session_end` read nothing from a session at all.

use crate::command::error::CommandError;
use crate::data::config::overlays::{ContextOverlaySpec, ContextScope};
use crate::data::config::EffectiveConfig;
use crate::data::fs::ContextDirResolver;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session};
use crate::engine::agent::agent_matrix::{matrix_for, SystemPromptMode};
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
        if let Some(name) = self.session.default_agent() {
            return Ok(name.clone());
        }
        AgentName::new("claude").map_err(CommandError::from)
    }

    /// Apply the ACP support and fallback policy after normal agent resolution.
    /// Explicit single-agent ACP intent is never downgraded.
    pub fn resolve_launch_mode(
        config: &EffectiveConfig,
        agent: &AgentName,
        explicit_single_agent_acp: bool,
    ) -> Result<LaunchModeDecision, EngineError> {
        use crate::data::config::global::LaunchModeFallback;
        use crate::data::config::repo::LaunchMode;

        if config.launch_mode() == LaunchMode::Stdio {
            return Ok(LaunchModeDecision::Stdio);
        }
        if crate::engine::agent::agent_matrix::matrix_for(agent.as_str())?.supports_acp {
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
    /// The note is a per-agent fact and lives in the one per-agent table
    /// (`AgentMatrix::deprecation_note`, F-32). `chat` and `exec prompt` each
    /// carried their own `if agent.as_str() == "gemini"` arm with a duplicated
    /// copy of the text; they now ask here.
    pub fn deprecation_warning(agent: &AgentName) -> Option<UserMessage> {
        matrix_for(agent.as_str())
            .ok()?
            .deprecation_note
            .map(|text| UserMessage {
                level: MessageLevel::Warning,
                text: text.to_string(),
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
        if let Ok(matrix) = matrix_for(agent.as_str()) {
            match &matrix.system_prompt_delivery {
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

    /// Emit deprecation warnings for legacy `envPassthrough` config fields.
    pub fn warn_legacy_config(&self, sink: &mut dyn UserMessageSink) {
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
    fn fallback_decision_matrix() {
        let supported = AgentName::new("cline").unwrap();
        let unsupported = AgentName::new("claude").unwrap();

        assert_eq!(
            LaunchPolicy::resolve_launch_mode(
                &config(LaunchModeFallback::Error),
                &supported,
                false
            )
            .unwrap(),
            LaunchModeDecision::Acp
        );
        assert!(matches!(
            LaunchPolicy::resolve_launch_mode(
                &config(LaunchModeFallback::Error),
                &unsupported,
                false
            ),
            Err(EngineError::AcpUnsupported { .. })
        ));
        assert_eq!(
            LaunchPolicy::resolve_launch_mode(
                &config(LaunchModeFallback::Stdio),
                &unsupported,
                false
            )
            .unwrap(),
            LaunchModeDecision::StdioWithFallbackWarning
        );
        assert!(matches!(
            LaunchPolicy::resolve_launch_mode(
                &config(LaunchModeFallback::Stdio),
                &unsupported,
                true
            ),
            Err(EngineError::AcpUnsupported { .. })
        ));
    }
}

#[cfg(test)]
mod warn_legacy_config_tests {
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
        LaunchPolicy::for_session(&session).warn_legacy_config(&mut sink);

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
        LaunchPolicy::for_session(&session).warn_legacy_config(&mut sink);

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
        LaunchPolicy::for_session(&session).warn_legacy_config(&mut sink);

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
