//! `ExecPromptCommand` — one-shot prompt injection.

use async_trait::async_trait;
use serde::Serialize;

use super::session_worktree::{report_kept_session_worktree, resolve_session_worktree};
use crate::command::commands::launch_policy::{
    ensure_agent_trusted, AgentPolicyRequest, LaunchModeDecision, LaunchPolicy, ModeRequest,
};
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};

mod flags;

pub use flags::ExecPromptCommandFlags;

#[derive(Debug, Clone, Serialize)]
pub struct ExecPromptOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation: Option<crate::data::automation_records::CommandAutomationResult>,
    /// The last accepted turn's native-confirmed answer (automation only).
    #[serde(skip)]
    pub automation_final_message: Option<String>,
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
    fn report_session_worktree(&mut self, _path: &std::path::Path, _branch: Option<&str>) {}

    /// The launch's workspace is mounted read-only (WI 0152). Called once,
    /// only for a read-only launch, after the `Info` message the command
    /// writes itself; `step` is always `None` for `exec prompt`. Presentation
    /// only: the decision is already made.
    fn report_workspace_access(
        &mut self,
        _access: crate::data::workspace::WorkspaceAccess,
        _step: Option<&str>,
    ) {
    }

    /// The resolved native start mode, mount access and transport of the
    /// launch (WI 0070). Called once, after the agent options resolve; `step`
    /// is always `None` for `exec prompt`. Presentation only.
    fn report_agent_policy(
        &mut self,
        _policy: crate::data::agent_policy::ResolvedAgentPolicy,
        _step: Option<&str>,
    ) {
    }
}

async fn ensure_exec_prompt_agent_setup(
    agent_engine: &crate::engine::agent::AgentEngine,
    session: &Session,
    agent: &AgentName,
    frontend: &mut Box<dyn ExecPromptCommandFrontend>,
) -> Result<(), CommandError> {
    let mut adapter =
        crate::command::commands::agent_setup::AgentFrontendAdapter::new(frontend.as_mut());
    agent_engine
        .ensure_available_for_session(session, agent, &mut adapter)
        .await
        .map_err(CommandError::from)
}

/// Build the final prompt string by combining an optional user-provided text
/// and optional resolved work item content. Content is prompt data only.
pub(crate) fn build_prompt_string(
    user_prompt: Option<&str>,
    work_item_content: Option<&str>,
) -> Option<String> {
    let user_prompt = user_prompt.filter(|p| !p.trim().is_empty());
    let work_item_content = work_item_content.filter(|p| !p.trim().is_empty());
    match (user_prompt, work_item_content) {
        (Some(user), Some(issue)) => Some(format!("{user}\n\n{issue}")),
        (Some(user), None) => Some(user.to_string()),
        // The prompt is the agent CLI's last positional argument. Work item
        // text comes from the repository, so it must never be parsed as an
        // option (`--dangerously-skip-permissions`, `--flag=value`).
        (None, Some(issue)) if issue.starts_with('-') => Some(format!("Work item:\n\n{issue}")),
        (None, Some(issue)) => Some(issue.to_string()),
        (None, None) => None,
    }
}

pub struct ExecPromptCommand {
    profile_request: super::launch_policy::settings_profile::ProfileRequest,
    flags: ExecPromptCommandFlags,
    engines: Engines,
    session: Session,
    /// Fired by an external cancel (the API's `POST /v1/worker/commands/{id}/cancel`).
    /// Never fired for the CLI or the TUI, which never install one.
    cancel: tokio_util::sync::CancellationToken,
    /// The host directory mounted read-write at `/awman/output` (WI 0137
    /// §3). Set only under the API; cleared at the start of every run, since
    /// a restarted `exec prompt` reruns from scratch.
    output_dir: Option<std::path::PathBuf>,
    /// `--read-only-workspace` (WI 0152). A pulled lease's read-only engine
    /// makes the launch read-only whatever this says.
    workspace_access: crate::data::workspace::WorkspaceAccess,
    /// `--prerequisite-identity` (the only prerequisite a prompt takes).
    prerequisites: crate::data::config::flags::PrerequisiteFlags,
}

impl ExecPromptCommand {
    pub fn new(flags: ExecPromptCommandFlags, engines: Engines, session: Session) -> Self {
        Self {
            profile_request: Default::default(),
            flags,
            engines,
            session,
            cancel: tokio_util::sync::CancellationToken::new(),
            output_dir: None,
            workspace_access: Default::default(),
            prerequisites: Default::default(),
        }
    }

    /// Grant the flag-selected git identity to the agent container.
    pub fn with_prerequisites(
        mut self,
        prerequisites: crate::data::config::flags::PrerequisiteFlags,
    ) -> Self {
        self.prerequisites = prerequisites;
        self
    }

    /// Mount the workspace read-only (`--read-only-workspace`, WI 0152).
    pub fn with_workspace_access(
        mut self,
        access: crate::data::workspace::WorkspaceAccess,
    ) -> Self {
        self.workspace_access = access;
        self
    }

    pub fn workspace_access(&self) -> crate::data::workspace::WorkspaceAccess {
        self.workspace_access
    }

    /// Observe `token`: once it fires the agent container is stopped and the
    /// command ends with exit code 130.
    pub fn with_cancellation(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    /// Mount `dir` at `/awman/output` into the agent container (WI 0137 §3).
    pub fn with_output_dir(mut self, dir: Option<std::path::PathBuf>) -> Self {
        self.output_dir = dir;
        self
    }

    pub fn with_profile(
        mut self,
        request: super::launch_policy::settings_profile::ProfileRequest,
    ) -> Self {
        self.profile_request = request;
        self
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
        if self.session.inline_work_item().is_none() {
            flags::require_task(
                self.flags.prompt.as_deref(),
                self.flags.work_item.as_deref(),
                &["exec", "prompt"],
            )?;
        }
        crate::command::automation::launch::validate_request(
            &["exec", "prompt"],
            self.flags.automation,
            &self.profile_request,
            self.flags.non_interactive,
        )?;
        let session = self.session;
        // Read-only workspace (WI 0152): decided and checked before anything
        // is written — outputs, work item staging, worktree, image. A pulled
        // lease's read-only engine makes the launch read-only too.
        let workspace_access = super::workspace_access::effective_access(
            self.workspace_access,
            &self.engines.agent_engine,
        );
        if let Err(e) = super::workspace_access::preflight_workspace_access(
            workspace_access,
            self.flags.allow_docker,
            self.engines.runtime.as_ref(),
        ) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec prompt: {e}"),
            });
            return Err(e);
        }
        let launch_engine =
            super::workspace_access::agent_engine_for(workspace_access, &self.engines.agent_engine);
        if self.output_dir.is_some() && !self.engines.runtime.capabilities().arbitrary_host_mounts {
            frontend.write_message(UserMessage { level: MessageLevel::Warning, text: "Persistent command outputs are unavailable on this runtime; /awman/output is not mounted.".into() });
        }

        // Each run of `exec prompt` is an attempt from scratch (a daemon
        // restart reruns it), so its outputs start empty (WI 0137 §3).
        if let Some(dir) = &self.output_dir {
            if let Err(e) = crate::data::fs::command_outputs::prepare_output_dir(dir, true) {
                return Err(CommandError::Other(format!(
                    "exec prompt: failed to prepare the outputs directory {}: {e}",
                    dir.display()
                )));
            }
        }

        // Resolve --work-item through the shared resolver (WI 0147). Any
        // resolution failure is a hard error before a container starts; the
        // resolver reports the source and title itself.
        let work_item_content = match crate::command::work_item::WorkItemResolver::new(
            session.clone(),
            std::sync::Arc::clone(&self.engines.git_engine),
        )
        .resolve(self.flags.work_item.as_deref(), &mut *frontend)
        .await
        {
            Ok(item) => item.map(|item| item.content),
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("exec prompt: failed to resolve work item: {e}"),
                });
                return Err(e.into());
            }
        };

        // Resolve first so staged inline input can supply the prompt too.
        let final_prompt =
            build_prompt_string(self.flags.prompt.as_deref(), work_item_content.as_deref())
                .ok_or_else(|| flags::require_task(None, None, &["exec", "prompt"]).unwrap_err())?;

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
        // Review/approve a custom definition before anything resolves it.
        if let Err(e) = ensure_agent_trusted(&session, &agent, frontend.as_mut()) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("exec prompt: {e}"),
            });
            return Err(e);
        }
        let invoker = session;
        let worktree = if let Some(target) = &self.flags.session_worktree {
            let resolved = resolve_session_worktree(
                target,
                &invoker,
                self.engines.git_engine.clone(),
                frontend.as_mut(),
            )
            .await;
            let worktree = resolved?;
            report_kept_session_worktree(&worktree, frontend.as_mut());
            frontend.report_session_worktree(&worktree.path, worktree.branch.as_deref());
            Some(worktree)
        } else {
            None
        };

        // Keep one reporting tail for all post-resolution returns, including
        // setup/launch/ACP errors. The worktree is always retained.
        let mut profile = None;
        let native_stream = crate::engine::agent_output::native_observation::NativeStream::new(
            self.profile_request.agent_session.clone(),
        );
        let mut observe_native = false;
        let mut automation_finished: Option<
            crate::command::automation::controller::FinishedExecution,
        > = None;
        let mut outcome = async {
            let session = match &worktree {
                Some(worktree) => Session::open_worktree_with_invoker_config(
                    worktree.path.clone(), worktree.path.clone(), &invoker,
                )?,
                None => invoker.clone(),
            };
            let image_session = match &worktree {
                Some(worktree) => Session::open_worktree_with_invoker_config(
                    worktree.invoker_root.clone(), worktree.invoker_root.clone(), &invoker,
                )?,
                None => invoker.clone(),
            };

            // Resolve ACP policy before overlays or container setup. A direct ACP
            // request is deliberately never eligible for fallback.
            let launch_decision = LaunchPolicy::for_session(&session).single_agent_launch_mode(
                &agent,
                &self.flags.agent,
                &self.flags.model,
                self.flags.launch_mode,
            )?;
            if launch_decision == LaunchModeDecision::StdioWithFallbackWarning {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Warning,
                    text: LaunchPolicy::acp_fallback_warning(&agent),
                });
            }
            // Native start mode, mount access and transport: one resolution,
            // before setup, credentials or mounts (WI 0070). The mount half
            // repeats the fail-fast preflight above and yields the same access.
            let agent_policy = match LaunchPolicy::for_session(&session).resolve_agent_policy(
                AgentPolicyRequest {
                    agent: &agent,
                    plan: self.flags.plan,
                    yolo: ModeRequest {
                        value: self.flags.yolo,
                        explicit: self.flags.yolo_authorized,
                    },
                    auto: ModeRequest {
                        value: self.flags.auto,
                        explicit: self.flags.auto_explicit,
                    },
                    requested_access: self.workspace_access,
                    launch: launch_decision,
                    allow_docker: self.flags.allow_docker,
                    non_interactive: self.flags.non_interactive,
                    model_requested: self.flags.model.is_some(),
                },
                &self.engines.agent_engine,
            ) {
                Ok(outcome) => outcome,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec prompt: {e}"),
                    });
                    return Err(e);
                }
            };
            for notice in &agent_policy.notices {
                frontend.write_message(notice.clone());
            }
            if let Some(note) = LaunchPolicy::deprecation_warning(&agent) {
                frontend.write_message(note);
            }

            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("exec prompt: using agent '{}'", agent.as_str()),
            });

            let cli_typed = match LaunchPolicy::parse_overlay_flags(&self.flags.overlay) {
                Ok(overlays) => overlays,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("exec prompt: invalid overlay spec: {e}"),
                    });
                    return Err(e);
                }
            };
            let collected = session
                .effective_config()
                .collected_overlays(cli_typed, None, None)?;

            // Emit deprecation warnings for legacy config fields.
            LaunchPolicy::for_session(&session)
                .warn_config(frontend.as_mut());

            let profile_run=AgentRunOptions {non_interactive:self.flags.non_interactive, model:self.flags.model.clone(), agent_flags:self.flags.agent_flags.clone(), launch_mode:if launch_decision==LaunchModeDecision::Acp {crate::data::config::agents::LaunchMode::Acp}else{crate::data::config::agents::LaunchMode::Stdio}, initial_prompt:Some(final_prompt.clone()),..Default::default()};
            let definition=launch_engine.resolve_definition(&session,&agent)?;
            if self.flags.automation.is_some() {
                crate::command::automation::launch::require_adapter(&["exec", "prompt"], &definition)?;
                if launch_decision == LaunchModeDecision::Acp {
                    return Err(CommandError::NativeCapabilityUnavailable(
                        "exec prompt --automation uses the native terminal or one-shot route, not ACP".into(),
                    ));
                }
            }
            // Interactive automation withholds the task from startup argv and
            // delivers it once, guarded, after readiness.
            let withhold_task = self.flags.automation.is_some() && !self.flags.non_interactive;
            profile=super::launch_policy::settings_profile::enter(&session,&agent,&self.profile_request,&profile_run,&definition,self.engines.runtime.clone()).await?;
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
                    &image_session,
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
            let directory_overlays = collected.directories;
            let mut command_mounts = Vec::new();
            let outputs_dir = self
                .output_dir
                .as_ref()
                .filter(|_| self.engines.runtime.capabilities().arbitrary_host_mounts);
            let system_prompt = match outputs_dir {
                Some(dir) => {
                    command_mounts.push(crate::data::fs::command_outputs::output_dir_overlay(dir));
                    Some(crate::engine::context_prompt::with_outputs_section(
                        system_prompt,
                        crate::engine::context_prompt::OutputsPromptRole::Prompt,
                    ))
                }
                None => system_prompt,
            };
            // The read-only section goes last, after the outputs section.
            let system_prompt = super::workspace_access::system_prompt_for(
                workspace_access,
                system_prompt,
                outputs_dir.is_some(),
                &context_overlays,
            );

            let mut run_opts = AgentRunOptions {
                native_session: self.profile_request.agent_session.clone(),
                profile_state: profile.as_ref().map(super::launch_policy::settings_profile::ProfileSession::run_state),
                command_secrets: self.engines.command_secrets(),
                // Resolved values: --plan suppresses an inherited yolo/auto.
                yolo: agent_policy.yolo.then_some(YoloMode::Enabled),
                yolo_authorized: self.flags.yolo_authorized,
                auto: agent_policy.auto.then_some(AutoMode::Enabled),
                plan: self.flags.plan.then_some(PlanMode::Enabled),
                allow_docker: self.flags.allow_docker,
                non_interactive: self.flags.non_interactive,
                structured_output: self.output_dir.is_some() || profile.is_some(),
                model: self.flags.model.clone(),
                agent_flags: self.flags.agent_flags.clone(),
                // ACP sends its initial turn after the initialize/session-new
                // handshake; stdio retains the existing seeded-launch behavior.
                initial_prompt: (launch_decision != LaunchModeDecision::Acp && !withhold_task)
                    .then(|| final_prompt.clone()),
                env_passthrough: if collected.env_passthrough.is_empty() {
                    None
                } else {
                    Some(collected.env_passthrough)
                },
                directory_overlays,
                command_mounts,
                include_all_skills: collected.include_all_skills,
                named_skills: collected.named_skills,
                system_prompt,
                context_overlays,
                launch_mode: match launch_decision {
                    LaunchModeDecision::Acp => crate::data::config::agents::LaunchMode::Acp,
                    _ => crate::data::config::agents::LaunchMode::Stdio,
                },
                // A fixed identity reaches the container as GIT_* env, which wins over
                // any repository or global `user.name`. A read-only prompt ignores it.
                grant_env: if workspace_access.is_read_only() {
                    Vec::new()
                } else {
                    crate::command::commands::workflow_prerequisites::granted_fixed_identity(
                        &crate::command::commands::workflow_prerequisites::policy_for(
                            &session,
                            &self.prerequisites,
                            None,
                        ),
                    )
                    .map(|identity| crate::engine::overlay::prerequisite_grants::identity_env(&identity))
                    .unwrap_or_default()
                },
                ..Default::default()
            };

            if let Some(worktree) = &worktree {
                if !self.engines.runtime.capabilities().arbitrary_host_mounts {
                    return Err(CommandError::Other(
                        "This runtime cannot mount the linked worktree's required Git directory".into(),
                    ));
                }
                run_opts.image_tag_override = Some(crate::data::image_tags::agent_image_tag(
                    &worktree.invoker_root, agent.as_str(),
                ));
                super::session_worktree::anchor_relative_overlays(
                    &mut run_opts.directory_overlays,
                    invoker.working_dir(),
                )?;
                if let Some(mut overlay) = worktree.git_directory_overlay(self.engines.git_engine.clone()).await? {
                    // The worktree's Git directory is part of the workspace:
                    // a read-only run must not commit or rewrite refs there.
                    super::workspace_access::restrict_to_access(&mut overlay, workspace_access);
                    run_opts.command_mounts.push(overlay);
                }
            }

            // WI 0070: an interactive automation launch mounts its
            // execution-private native lifecycle channel.
            let automation_channel = match profile.as_ref() {
                Some(profile) if self.flags.automation.is_some() && withhold_task => crate::command::automation::launch::prepare_channel(
                    &session,
                    &definition,
                    profile,
                    !self.engines.runtime.capabilities().kit_declarative,
                )?,
                _ => None,
            };
            run_opts.automation_channel = automation_channel.clone();

            let resolved = match launch_engine.resolve_agent_options(
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
            for note in LaunchPolicy::launch_notices(&agent, &run_opts) {
                frontend.write_message(note);
            }
            if let Some(message) =
                LaunchPolicy::native_mode_message(&agent, &agent_policy.policy, None)
            {
                frontend.write_message(message);
            }
            frontend.report_agent_policy(agent_policy.policy, None);

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
            if workspace_access.is_read_only() {
                frontend.write_message(super::workspace_access::read_only_message(None));
                frontend.report_workspace_access(workspace_access, None);
            }
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("Launching agent ({})…", self.engines.runtime.display_name()),
            });
            if let Some(profile)=profile.as_mut() {
                let redaction = instance.output_redaction();
                profile.bind_redaction(redaction)?;
                profile.bind_resource(&instance.handle_preview().name)?;
            }
            let mut cancelled = false;
            // Headless automation reserves its one durable turn before the
            // provider one-shot runs (B1).
            let mut headless_run: Option<crate::command::automation::launch::HeadlessRun> = None;
            let exit = if let (Some(mode), true) = (self.flags.automation, withhold_task) {
                let run = crate::command::automation::launch::start(
                    crate::command::automation::launch::StartRequest {
                        command: &["exec", "prompt"],
                        session: &session,
                        definition: &definition,
                        profile: profile.as_ref().expect("automation requires a settings profile"),
                        policy: &agent_policy.policy,
                        mode,
                        requested_native: self.profile_request.agent_session.clone(),
                        runtime: self.engines.runtime.runtime_name().to_string(),
                        host: frontend.automation_host(),
                        initial_task: Some(final_prompt.clone()),
                        interactive: true,
                        channel: automation_channel.clone(),
                    },
                )?;
                let cancel_timeout = session.effective_config().automation_timeouts()?.cancel.to_duration();
                frontend.set_pty_active(true);
                let container_frontend = frontend.container_frontend_for_pty();
                let (exit, finished) = crate::command::automation::launch::run_interactive(
                    run,
                    instance,
                    container_frontend,
                    self.engines.runtime.clone(),
                    self.cancel.clone(),
                    cancel_timeout,
                )
                .await;
                frontend.set_pty_active(false);
                frontend.wait_output_drained().await;
                frontend.replay_queued();
                cancelled = finished.cancelled;
                automation_finished = Some(finished);
                exit
            } else if launch_decision == LaunchModeDecision::Acp {
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
                let acp_handle = execution.handle().clone();
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
                )
                .with_agent_name(agent.as_str());
                // The handshake and every turn race an external cancel, as the
                // stdio wait does.
                let driven = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => None,
                    driven = async {
                        acp.initialize("/workspace", agent_policy.acp_mode_id.as_deref())
                            .await?;
                        // The seeded prompt is the first ACP turn, driven inside
                        // the same loop as follow-ups so its `session/update`s are
                        // rendered (the subscription is taken before the prompt is
                        // sent) and any permission request during that turn is
                        // serviced.
                        Ok::<_, crate::engine::error::EngineError>(acp
                            .drive_with_initial_prompt(frontend.as_mut(), Some(final_prompt))
                            .await)
                    } => Some(driven),
                };
                match driven {
                    Some(Ok(exit)) => exit,
                    Some(Err(e)) => {
                        // Reap the launched container before returning on a failed
                        // handshake so it is not left running.
                        let _ = acp.shutdown().await;
                        return Err(CommandError::from(e));
                    }
                    None => {
                        cancelled = true;
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text: "exec prompt: cancelled; stopping the agent".to_string(),
                        });
                        // Stopped through the runtime like `agents kill`, then
                        // reaped: the ACP client process alone is not the container.
                        if let Err(e) = crate::command::commands::agents::kill::stop_and_remove_agent(
                            self.engines.runtime.as_ref(),
                            &acp_handle,
                        ) {
                            tracing::warn!(error = %e, "cancel: stopping the exec prompt agent failed");
                        }
                        acp.shutdown().await
                    }
                }
            } else {
                frontend.set_pty_active(true);
                let mut container_frontend = frontend.container_frontend_for_pty();
                if profile.is_some() && run_opts.non_interactive {
                    observe_native=true;
                    container_frontend=crate::engine::agent_output::native_observation::with_native_observation(container_frontend,definition.output_format,native_stream.clone());
                }
                if let (Some(mode), false) = (self.flags.automation, withhold_task) {
                    let mut run = crate::command::automation::launch::HeadlessRun::start(
                        crate::command::automation::launch::StartRequest {
                            command: &["exec", "prompt"],
                            session: &session,
                            definition: &definition,
                            profile: profile.as_ref().expect("automation requires a settings profile"),
                            policy: &agent_policy.policy,
                            mode,
                            requested_native: self.profile_request.agent_session.clone(),
                            runtime: self.engines.runtime.runtime_name().to_string(),
                            host: frontend.automation_host(),
                            initial_task: None,
                            interactive: false,
                            channel: None,
                        },
                        &final_prompt,
                    )?;
                    run.launching()?;
                    headless_run = Some(run);
                }
                if headless_run.is_some() {
                    container_frontend =
                        crate::command::automation::frontend::with_lifecycle_deadline(container_frontend);
                }
                let mut execution = match instance.run_with_frontend(container_frontend) {
                    Ok(e) => e,
                    Err(e) => {
                        let mut storage_error = None;
                        if let Some(run) = headless_run.take() {
                            // The spawn failed: the reserved turn ends failed
                            // (never replayed), the execution ends.
                            let failed = Err(crate::engine::error::EngineError::Other(e.to_string()));
                            let finished = run.finish(&native_stream, &failed, false).await;
                            if let Err(error @ CommandError::Data(_)) =
                                crate::command::automation::launch::into_command_result(&finished)
                            {
                                storage_error = Some(error);
                            }
                        }
                        frontend.set_pty_active(false);
                        frontend.replay_queued();
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("exec prompt: agent launch failed: {e}"),
                        });
                        return Err(storage_error.unwrap_or_else(|| CommandError::from(e)));
                    }
                };
                let running_error = headless_run.as_ref().and_then(|run| run.running().err());
                frontend.set_stuck_sender(execution.stuck_sender());
                // Raced against an external cancel, the way a workflow step's
                // wait is: the execution moves into a wait task, and a cancel
                // kills it and stops it through the runtime like `agents kill`.
                let handle = execution.handle().clone();
                let mut cancel_handle = execution.cancel_handle();
                let (wait_tx, mut wait_rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let _ = wait_tx.send(execution.wait().await);
                });
                let dropped = || {
                    Err(crate::engine::error::EngineError::Other(
                        "exec prompt wait task dropped unexpectedly".into(),
                    ))
                };
                // Automation: the saved startup deadline replaces the
                // first-byte grace (no identity by then fails the launch),
                // and an accepted stop is bounded by the saved cancellation
                // budget, with the runtime's blocking stop kept off the
                // executor. Ordinary runs keep their previous behavior.
                let startup_expiry: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                    match &headless_run {
                        Some(run) => Box::pin(run.startup_expiry(&native_stream)),
                        None => Box::pin(std::future::pending()),
                    };
                let cancel_budget = headless_run.as_ref().map(|run| run.cancel_timeout());
                enum Interrupt {
                    Cancel,
                    Startup,
                    Storage,
                }
                let first = if running_error.is_some() { Err(Interrupt::Storage) } else { tokio::select! {
                    biased;
                    result = &mut wait_rx => Ok(result.unwrap_or_else(|_| dropped())),
                    _ = async {tokio::select! {_=self.cancel.cancelled()=>{},_=native_stream.mismatch.cancelled()=>{}}} => Err(Interrupt::Cancel),
                    _ = startup_expiry => Err(Interrupt::Startup),
                } };
                let exit = match first {
                    Ok(result) => result,
                    Err(why) => {
                        let text = match why {
                            Interrupt::Storage => "exec prompt: storage failed; stopping the agent".to_string(),
                            Interrupt::Cancel => {
                                cancelled = true;
                                "exec prompt: cancelled; stopping the agent".to_string()
                            }
                            Interrupt::Startup => {
                                if let Some(run) = headless_run.as_mut() {
                                    run.startup_expired();
                                }
                                "exec prompt: the agent did not start before the automation startup deadline; stopping it".to_string()
                            }
                        };
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Warning,
                            text,
                        });
                        let deadline = cancel_budget.map(|b| tokio::time::Instant::now() + b);
                        if let Some(ch) = cancel_handle.take() {
                            let interrupt = tokio::task::spawn_blocking(move || ch.cancel());
                            match deadline {
                                None => drop(interrupt.await),
                                // Bounded: a hung runtime cannot hold the deadline.
                                Some(deadline) => drop(tokio::time::timeout_at(deadline, interrupt).await),
                            }
                        }
                        let runtime = self.engines.runtime.clone();
                        let stop_handle = handle.clone();
                        let stop = tokio::task::spawn_blocking(move || {
                            crate::command::commands::agents::kill::stop_and_remove_agent(
                                runtime.as_ref(),
                                &stop_handle,
                            )
                        });
                        match deadline {
                            None => {
                                if let Ok(Err(e)) = stop.await {
                                    tracing::warn!(error = %e, "cancel: stopping the exec prompt agent failed");
                                }
                                (&mut wait_rx).await.unwrap_or_else(|_| dropped())
                            }
                            Some(deadline) => {
                                if let Ok(Ok(Err(e))) = tokio::time::timeout_at(deadline, stop).await {
                                    tracing::warn!(error = %e, "cancel: stopping the exec prompt agent failed");
                                }
                                match tokio::time::timeout_at(deadline, &mut wait_rx).await {
                                    Ok(result) => result.unwrap_or_else(|_| dropped()),
                                    Err(_) => {
                                        // Publish by the deadline; the owned
                                        // cleanup obligation stays pending.
                                        if let Some(run) = headless_run.as_mut() {
                                            run.cleanup_pending();
                                        }
                                        Err(crate::engine::error::EngineError::Other(
                                            "the agent did not stop before the cancellation deadline".into(),
                                        ))
                                    }
                                }
                            }
                        }
                    }
                };
                frontend.set_pty_active(false);
                frontend.wait_output_drained().await;
                frontend.replay_queued();
                if let Some(error) = running_error { return Err(error); }
                exit
            };

            crate::command::commands::LaunchPolicy::report_session_end(
                frontend.as_mut(),
                "exec prompt",
                &exit,
            );

            if let Some(run) = headless_run.take() {
                automation_finished = Some(run.finish(&native_stream, &exit, cancelled).await);
            }
            if let Some(finished) = &automation_finished {
                crate::command::automation::launch::into_command_result(finished)?;
            }
            let exit_code = crate::command::dispatch::outcomes::agent_exit_code(exit, cancelled)?;
            Ok(ExecPromptOutcome {
                automation: automation_finished.as_ref().map(|f| f.automation.clone()),
                automation_final_message: automation_finished.as_ref().and_then(|f| f.final_message.clone()),
                agent: Some(agent.as_str().to_string()),
                exit_code,
            })
        }
        .await;
        if observe_native && !native_stream.exact_resume_confirmed() {
            outcome = Err(CommandError::NativeSessionNotFound);
        }
        if let Some(mut profile) = profile {
            if observe_native && !native_stream.exact_resume_confirmed() {
                if let Err(error) = profile.reject_native_identity() {
                    outcome = Err(error);
                }
            }
            if let Some(id) = native_stream.identity() {
                if let Err(error) = profile.observe_native_session(id) {
                    outcome = Err(error);
                }
            }
            if let Some(id) = automation_finished
                .as_ref()
                .and_then(|f| f.automation.native_session_id.clone())
                .filter(|id| native_stream.identity().as_ref() != Some(id))
            {
                if let Err(error) = profile.observe_native_session(id) {
                    outcome = Err(error);
                }
            }
            let successful_exit = outcome
                .as_ref()
                .is_ok_and(|result| result.exit_code.unwrap_or_default() == 0);
            let turn = if native_stream.complete() && successful_exit {
                crate::data::fs::agent_profiles::TurnStateAtExit::Complete
            } else if !successful_exit {
                crate::data::fs::agent_profiles::TurnStateAtExit::Interrupted
            } else {
                crate::data::fs::agent_profiles::TurnStateAtExit::Unknown
            };
            match profile.leave(self.engines.runtime.clone(), turn).await {
                Ok(m) if m.history_unavailable.as_deref() == Some("output_incomplete") => {
                    outcome = Err(CommandError::Other(
                        "output_incomplete: profile history withheld".into(),
                    ));
                }
                Ok(m) if m.state.admits_writer() => {}
                result => {
                    let detail = result.err().map(|e| e.to_string()).unwrap_or_default();
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Warning,
                        text: format!(
                            "Profile cleanup remains unconfirmed; run awman agents profile close. {detail}"
                        ),
                    });
                    // An automation command never reports success or
                    // complete cleanup over an unconfirmed profile cleanup.
                    if let Some(finished) = &automation_finished {
                        outcome = Err(crate::command::automation::launch::cleanup_unconfirmed(
                            finished,
                            outcome.err(),
                            &detail,
                        ));
                    }
                }
            }
        }
        if let Some(worktree) = &worktree {
            report_kept_session_worktree(worktree, frontend.as_mut());
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::build_prompt_string;

    #[test]
    fn work_item_text_alone_never_starts_with_an_option_dash() {
        for content in ["--dangerously-skip-permissions", "-p=x\nrest"] {
            let prompt = build_prompt_string(None, Some(content)).unwrap();
            assert!(!prompt.starts_with('-'), "{prompt:?}");
            assert!(prompt.ends_with(content));
        }
        assert_eq!(
            build_prompt_string(None, Some("# Title\n\nbody")).as_deref(),
            Some("# Title\n\nbody")
        );
        assert_eq!(
            build_prompt_string(Some("do it"), Some("-x")).as_deref(),
            Some("do it\n\n-x")
        );
    }
}
