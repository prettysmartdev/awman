//! `ExecPromptCommand` — one-shot prompt injection.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::agent_auth::AgentAuthFrontend;
use crate::command::commands::agent_setup::AgentSetupFrontend;
use crate::command::commands::mount_scope::MountScopeFrontend;
use crate::command::commands::{
    collect_all_overlay_specs, parse_overlay_list, resolve_agent, resolve_context_overlays,
    warn_legacy_config, Command,
};
use crate::command::dispatch::Engines;
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
    pub overlay: Vec<String>,
    pub issue_source: crate::data::issue::IssueSourceFlags,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecPromptOutcome {
    pub agent: Option<String>,
    pub exit_code: Option<i32>,
}

pub trait ExecPromptCommandFrontend:
    UserMessageSink
    + MountScopeFrontend
    + AgentSetupFrontend
    + AgentAuthFrontend
    + crate::command::commands::agent_setup::HasAgentFrontend
    + Send
    + Sync
{
    /// Inform the frontend that the host stdio is now owned by a running
    /// container. Frontends that would otherwise interleave UserMessages with
    /// container output (e.g. the CLI) queue messages until the container
    /// releases stdio. Default impl: no-op (suitable for non-blocking sinks
    /// like the TUI).
    fn set_pty_active(&mut self, _active: bool) {}

    /// Called after the agent container launches. The sender is the broadcast
    /// channel from the container's stuck detector; the TUI stores it so the
    /// tab can subscribe for stuck-coloring. Default impl: no-op (CLI/API
    /// frontends ignore it).
    fn set_stuck_sender(
        &mut self,
        _sender: std::sync::Arc<
            tokio::sync::broadcast::Sender<crate::engine::agent_runtime::StuckEvent>,
        >,
    ) {
    }
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
    let runtime = std::sync::Arc::clone(agent_engine.container_runtime_arc());
    agent_engine
        .ensure_available(session, agent, &config, &mut adapter, move |tag: &str| {
            runtime.image_exists(tag)
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
        // Agent execution under a sandbox-class runtime lands in WI 0090;
        // until then the stub surfaces NotImplemented instead of panicking
        // or silently falling back to Docker.
        self.engines
            .require_container_runtime()
            .map_err(CommandError::from)?;

        let session = self.session;

        // Validate that at least one of prompt and --issue is provided.
        if self.flags.prompt.is_none() && self.flags.issue_source.issue.is_none() {
            return Err(CommandError::Other(
                "exec prompt: either a prompt argument or --issue must be provided".to_string(),
            ));
        }

        // Resolve issue if --issue was provided.
        let issue_markdown = if let Some(ref issue_ref) = self.flags.issue_source.issue {
            let router = crate::data::issue::router::IssueSourceRouter::default();
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

        let agent = match resolve_agent(&self.flags.agent, &session) {
            Ok(a) => a,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: failed to resolve agent: {e}"),
                });
                return Err(e);
            }
        };
        if agent.as_str() == "gemini" {
            frontend.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: "The 'gemini' agent is deprecated by Google. \
                       Migrate to 'antigravity' — run 'awman chat antigravity' \
                       (or 'awman config set agent antigravity' to change your default)."
                    .to_string(),
            });
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
        let collected = collect_all_overlay_specs(&session, cli_typed, None, None)?;

        // Emit deprecation warnings for legacy config fields.
        warn_legacy_config(&session, frontend.as_mut());

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

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Resolving agent credentials…".into(),
        });
        let credentials = match self
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

        let (context_overlays, system_prompt) = resolve_context_overlays(
            &collected.context_overlays,
            &session,
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
            initial_prompt: Some(final_prompt),
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
            ..Default::default()
        };

        let mut options = match self
            .engines
            .agent_engine
            .build_options(&session, &agent, &run_opts)
        {
            Ok(o) => o,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: failed to build container options: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        if !credentials.env_vars.is_empty() {
            options.push(
                crate::engine::container::options::ContainerOption::AgentCredentials {
                    env_vars: credentials.env_vars,
                },
            );
        }

        let instance = match crate::engine::agent_runtime::ResolvedAgentOptions::container(options)
            .and_then(|o| self.engines.runtime.build(o))
        {
            Ok(i) => i,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: failed to build container: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Launching agent container…".into(),
        });
        frontend.set_pty_active(true);
        let container_frontend = frontend.container_frontend_for_pty();
        let mut execution = match instance.run_with_frontend(container_frontend) {
            Ok(e) => e,
            Err(e) => {
                frontend.set_pty_active(false);
                frontend.replay_queued();
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: container launch failed: {e}"),
                });
                return Err(CommandError::from(e));
            }
        };
        // Publish the stuck sender so the TUI can color the tab when the
        // agent stops producing output (mirrors the workflow engine's
        // set_stuck_sender call after each step launch).
        frontend.set_stuck_sender(execution.stuck_sender());
        let exit = execution.wait().await;
        frontend.set_pty_active(false);
        frontend.replay_queued();

        let exit_code = exit.map(|e| e.exit_code).ok();
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Agent session ended".into(),
        });
        Ok(ExecPromptOutcome {
            agent: Some(agent.as_str().to_string()),
            exit_code,
        })
    }
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
        use crate::data::issue::{Issue, IssueSource, IssueSourceError};
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
