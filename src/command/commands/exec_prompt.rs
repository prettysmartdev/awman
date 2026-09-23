//! `ExecPromptCommand` — one-shot prompt injection.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::launch_policy::{LaunchModeDecision, LaunchPolicy};
use crate::command::commands::{parse_overlay_list, Command};
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};

#[derive(Debug, Clone)]
pub struct ExecPromptCommandFlags {
    pub prompt: Option<String>,
    pub non_interactive: bool,
    pub plan: bool,
    pub allow_docker: bool,
    pub yolo: bool,
    pub auto: bool,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub launch_mode: Option<crate::data::config::repo::LaunchMode>,
    pub overlay: Vec<String>,
    pub issue_source: crate::engine::issue::IssueSourceFlags,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecPromptOutcome {
    pub agent: Option<String>,
    pub exit_code: Option<i32>,
}

pub trait ExecPromptCommandFrontend:
    UserMessageSink
    + crate::command::commands::agent_setup::AgentLaunchFrontend
    + crate::engine::acp::AcpFrontend
    + Send
    + Sync
{
}

async fn ensure_exec_prompt_agent_setup(
    agent_engine: &crate::engine::agent::AgentEngine,
    session: &Session,
    agent: &AgentName,
    frontend: &mut Box<dyn ExecPromptCommandFrontend>,
) -> Result<(), CommandError> {
    use crate::data::config::effective::EffectiveConfig;
    let config = EffectiveConfig::default();
    let mut adapter =
        crate::command::commands::agent_setup::AgentFrontendAdapter::new(frontend.as_mut());
    let runtime = std::sync::Arc::clone(agent_engine.runtime());
    agent_engine
        .ensure_available(session, agent, &config, &mut adapter, move |tag: &str| {
            runtime.image_exists(tag).unwrap_or(false)
        })
        .await
        .map_err(CommandError::from)
}

/// Build the final prompt string by combining an optional user-provided text
/// and an optional issue markdown block. Exposed for unit testing.
pub(crate) fn build_prompt_string(
    user_prompt: Option<&str>,
    issue_markdown: Option<&str>,
) -> Option<String> {
    match (user_prompt, issue_markdown) {
        (Some(user), Some(issue)) => Some(format!("{user}\n\n{issue}")),
        (Some(user), None) => Some(user.to_string()),
        (None, Some(issue)) => Some(issue.to_string()),
        (None, None) => None,
    }
}

pub struct ExecPromptCommand {
    flags: ExecPromptCommandFlags,
    engines: Engines,
    session: Session,
}

impl ExecPromptCommand {
    pub fn new(flags: ExecPromptCommandFlags, engines: Engines, session: Session) -> Self {
        Self {
            flags,
            engines,
            session,
        }
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10). A
    /// whitespace-only positional prompt is normalised to `None`; the
    /// prompt-or-issue requirement is checked at run time, where `--issue`
    /// can still supply the text.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let prompt = match ctx.args.get("prompt") {
            Some(prompt) if prompt.trim().is_empty() => None,
            other => other.map(str::to_string),
        };
        Ok(Self::new(
            ExecPromptCommandFlags {
                prompt,
                non_interactive: ctx.flags.bool("non-interactive"),
                plan: ctx.flags.bool("plan"),
                allow_docker: ctx.flags.bool("allow-docker"),
                yolo: ctx.flags.bool("yolo"),
                auto: ctx.flags.bool("auto"),
                agent: ctx.flags.string("agent"),
                model: ctx.flags.string("model"),
                launch_mode: crate::command::dispatch::parse_launch_mode(
                    ctx.flags.string("launch-mode"),
                    &ctx.path(),
                )?,
                overlay: ctx.flags.strs("overlay").to_vec(),
                issue_source: crate::engine::issue::IssueSourceFlags {
                    issue: ctx.flags.string("issue"),
                },
            },
            ctx.engines.clone(),
            ctx.session.clone(),
        ))
    }

    pub fn flags(&self) -> &ExecPromptCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ExecPromptCommand {
    type Frontend = Box<dyn ExecPromptCommandFrontend>;
    type Outcome = ExecPromptOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let session = self.session;

        // Validate that at least one of prompt and --issue is provided.
        if self.flags.prompt.is_none() && self.flags.issue_source.issue.is_none() {
            return Err(CommandError::Other(
                "exec prompt: either a prompt argument or --issue must be provided".to_string(),
            ));
        }

        // Resolve issue if --issue was provided.
        let issue_markdown = if let Some(ref issue_ref) = self.flags.issue_source.issue {
            let router = crate::engine::issue::router::IssueSourceRouter::new(
                std::sync::Arc::clone(&self.engines.git_engine),
                session.env(),
            );
            match router.fetch_issue_with_progress(issue_ref, session.git_root(), &mut *frontend) {
                Ok((issue, source)) => {
                    let md = source.format_as_markdown(&issue);
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Info,
                        text: format!("exec prompt: fetched issue '{}'", issue.title),
                    });
                    Some(md)
                }
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec prompt: failed to fetch issue: {e}"),
                    });
                    return Err(CommandError::Other(e.to_string()));
                }
            }
        } else {
            None
        };

        // Construct the final prompt string. The earlier validation above
        // guarantees at least one input is present, so this cannot be None.
        let final_prompt =
            build_prompt_string(self.flags.prompt.as_deref(), issue_markdown.as_deref())
                .expect("validated above: at least one of prompt or --issue must be present");

        let agent = match LaunchPolicy::for_session(&session).resolve_agent(&self.flags.agent) {
            Ok(a) => a,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: failed to resolve agent: {e}"),
                });
                return Err(e);
            }
        };
        // Resolve ACP policy before overlays or container setup. A direct ACP
        // request is deliberately never eligible for fallback.
        let config = command_effective_config(&session, &self.flags);
        let explicit_acp = self.flags.launch_mode
            == Some(crate::data::config::repo::LaunchMode::Acp)
            || (self.flags.agent.is_none()
                && session.repo_config().agent.is_some()
                && session.repo_config().launch_mode
                    == Some(crate::data::config::repo::LaunchMode::Acp));
        let launch_decision = match LaunchPolicy::resolve_launch_mode(&config, &agent, explicit_acp)
        {
            Ok(decision) => decision,
            Err(e) => return Err(CommandError::from(e)),
        };
        if launch_decision == LaunchModeDecision::StdioWithFallbackWarning {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: LaunchPolicy::acp_fallback_warning(&agent),
            });
        }
        if let Some(note) = LaunchPolicy::deprecation_warning(&agent) {
            frontend.write_message(note);
        }

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("exec prompt: using agent '{}'", agent.as_str()),
        });

        let cli_typed = {
            let mut all = Vec::new();
            for s in &self.flags.overlay {
                match parse_overlay_list(s) {
                    Ok(parsed) => all.extend(parsed),
                    Err(reason) => {
                        let e = CommandError::InvalidOverlaySpec {
                            spec: s.clone(),
                            reason,
                        };
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("exec prompt: invalid overlay spec: {e}"),
                        });
                        return Err(e);
                    }
                }
            }
            all
        };
        let collected = session
            .effective_config()
            .collected_overlays(cli_typed, None, None)?;

        // Emit deprecation warnings for legacy config fields.
        LaunchPolicy::for_session(&session).warn_legacy_config(frontend.as_mut());

        // The Dockerfile + image setup is container-paradigm only; sandbox
        // runtimes get their per-agent kits from `awman ready` instead.
        if self.engines.runtime.capabilities().kit_declarative {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "exec prompt: {} runtime active — using the agent kit prepared by \
                     `awman ready` (no image build needed)",
                    self.engines.runtime.display_name()
                ),
            });
        } else {
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: "Checking agent availability…".into(),
            });
            if let Err(e) = ensure_exec_prompt_agent_setup(
                self.engines.agent_engine.as_ref(),
                &session,
                &agent,
                &mut frontend,
            )
            .await
            {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: agent setup failed: {e}"),
                });
                return Err(e);
            }
        }

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Resolving agent credentials…".into(),
        });
        let resolved_credentials = match self
            .engines
            .auth_engine
            .resolve_agent_auth(&session, &agent)
        {
            Ok(c) => c,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: credential resolution failed: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        // File delivery is container-only; preserve sbx's existing env-only
        // Claude credential path without touching its auth implementation.
        let credentials = if self.engines.runtime.capabilities().kit_declarative
            && matches!(
                resolved_credentials.delivery,
                crate::engine::auth::CredentialDelivery::File(_)
            ) {
            self.engines.auth_engine.agent_env_credentials(&agent)?
        } else {
            resolved_credentials
        };

        let (context_overlays, system_prompt) = LaunchPolicy::for_session(&session)
            .with_git(&self.engines.git_engine)
            .resolve_context_overlays(
                &collected.context_overlays,
                &agent,
                None,
                None,
                frontend.as_mut(),
            )?;

        let run_opts = AgentRunOptions {
            yolo: self.flags.yolo.then_some(YoloMode::Enabled),
            auto: self.flags.auto.then_some(AutoMode::Enabled),
            plan: self.flags.plan.then_some(PlanMode::Enabled),
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            model: self.flags.model.clone(),
            // ACP sends its initial turn after the initialize/session-new
            // handshake; stdio retains the existing seeded-launch behavior.
            initial_prompt: (launch_decision != LaunchModeDecision::Acp)
                .then(|| final_prompt.clone()),
            env_passthrough: if collected.env_passthrough.is_empty() {
                None
            } else {
                Some(collected.env_passthrough)
            },
            directory_overlays: collected.directories,
            include_all_skills: collected.include_all_skills,
            named_skills: collected.named_skills,
            system_prompt,
            context_overlays,
            launch_mode: match launch_decision {
                LaunchModeDecision::Acp => crate::data::config::repo::LaunchMode::Acp,
                _ => crate::data::config::repo::LaunchMode::Stdio,
            },
            ..Default::default()
        };

        let resolved = match self.engines.agent_engine.resolve_agent_options(
            &session,
            &agent,
            &run_opts,
            &credentials,
        ) {
            Ok(o) => o,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: failed to build agent options: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };

        let instance = match self.engines.runtime.build(resolved) {
            Ok(i) => i,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: failed to build agent: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!("Launching agent ({})…", self.engines.runtime.display_name()),
        });
        let exit = if launch_decision == LaunchModeDecision::Acp {
            let (runtime_frontend, transport) = crate::engine::acp::AcpTransport::channel();
            let execution = match instance.run_with_frontend(Box::new(runtime_frontend)) {
                Ok(execution) => execution,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec prompt: failed to launch ACP agent: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };
            let mut acp = crate::engine::acp::AcpSession::from_transport(
                execution,
                transport,
                Box::new(crate::data::message::StderrMessageSink::new()),
                // Layer 2 owns the flags, so Layer 2 turns them into the
                // permission policy (F-46).
                if matches!(run_opts.yolo, Some(YoloMode::Enabled))
                    || matches!(run_opts.auto, Some(AutoMode::Enabled))
                {
                    crate::engine::acp::PermissionPolicy::AutoApprove
                } else {
                    crate::engine::acp::PermissionPolicy::Ask
                },
            );
            if let Err(e) = acp.initialize("/workspace").await {
                // Reap the launched container before returning on a failed
                // handshake so it is not left running.
                let _ = acp.shutdown().await;
                return Err(CommandError::from(e));
            }
            // The seeded prompt is the first ACP turn, driven inside the same
            // loop as follow-ups so its `session/update`s are rendered (the
            // subscription is taken before the prompt is sent) and any
            // permission request during that turn is serviced.
            acp.drive_with_initial_prompt(frontend.as_mut(), Some(final_prompt))
                .await
        } else {
            frontend.set_pty_active(true);
            let container_frontend = frontend.container_frontend_for_pty();
            let mut execution = match instance.run_with_frontend(container_frontend) {
                Ok(e) => e,
                Err(e) => {
                    frontend.set_pty_active(false);
                    frontend.replay_queued();
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec prompt: agent launch failed: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };
            frontend.set_stuck_sender(execution.stuck_sender());
            let exit = execution.wait().await;
            frontend.set_pty_active(false);
            frontend.replay_queued();
            exit
        };

        crate::command::commands::LaunchPolicy::report_session_end(
            frontend.as_mut(),
            "exec prompt",
            &exit,
        );

        let exit_code = exit.map(|e| e.exit_code).ok();
        Ok(ExecPromptOutcome {
            agent: Some(agent.as_str().to_string()),
            exit_code,
        })
    }
}

fn command_effective_config(
    session: &Session,
    command_flags: &ExecPromptCommandFlags,
) -> crate::data::config::effective::EffectiveConfig {
    let current = session.effective_config();
    let mut flags = current.flags().clone();
    flags.agent = command_flags.agent.clone();
    flags.model = command_flags.model.clone();
    flags.launch_mode = command_flags.launch_mode;
    crate::data::config::effective::EffectiveConfig::new(
        flags,
        current.env().clone(),
        current.repo().clone(),
        current.global().clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_only_user_text() {
        let result = build_prompt_string(Some("my prompt"), None);
        assert_eq!(result, Some("my prompt".to_string()));
    }

    #[test]
    fn build_prompt_only_issue_text() {
        let result = build_prompt_string(None, Some("# Issue Title\n\nBody"));
        assert_eq!(result, Some("# Issue Title\n\nBody".to_string()));
    }

    #[test]
    fn build_prompt_both_user_and_issue_separated_by_double_newline() {
        let result = build_prompt_string(Some("context"), Some("# Issue"));
        assert_eq!(result, Some("context\n\n# Issue".to_string()));
    }

    #[test]
    fn build_prompt_both_absent_returns_none() {
        let result = build_prompt_string(None, None);
        assert_eq!(result, None);
    }

    #[test]
    fn build_prompt_issue_with_empty_body_uses_title_only_markdown() {
        // format_as_markdown with empty body produces "# Title Only".
        use crate::engine::issue::{Issue, IssueSource, IssueSourceError};
        use std::path::Path;

        struct FakeSource;
        impl IssueSource for FakeSource {
            fn provider_name(&self) -> &str {
                "Test"
            }
            fn provider_prefix(&self) -> &str {
                "tst"
            }
            fn issue_identifier(&self, _: &Issue) -> String {
                "0".into()
            }
            fn can_handle(&self, _: &str) -> bool {
                false
            }
            fn fetch_issue(&self, _: &str, _: &Path) -> Result<Issue, IssueSourceError> {
                unimplemented!()
            }
        }
        let issue = Issue {
            source_id: String::new(),
            title: "Title Only".into(),
            body: String::new(),
            provider: "Test".into(),
        };
        let md = FakeSource.format_as_markdown(&issue);
        assert_eq!(md, "# Title Only");

        // When used as the issue_markdown argument, no trailing whitespace.
        let combined = build_prompt_string(Some("user text"), Some(&md));
        assert_eq!(combined, Some("user text\n\n# Title Only".to_string()));

        // When used alone, no trailing whitespace.
        let alone = build_prompt_string(None, Some(&md));
        assert_eq!(alone, Some("# Title Only".to_string()));
    }
}
