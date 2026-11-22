//! `ChatCommand` — freeform chat with the configured agent.

use async_trait::async_trait;
use serde::Serialize;

use super::session_worktree::{report_kept_session_worktree, resolve_session_worktree};
use crate::command::commands::launch_policy::{
    ensure_agent_trusted, AgentPolicyRequest, LaunchModeDecision, LaunchPolicy, ModeRequest,
};
use crate::command::commands::mount_scope::MountScope;
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::{AutoMode, PlanMode, YoloMode};

mod flags;
pub use flags::ChatCommandFlags;

#[derive(Debug, Clone, Serialize)]
pub struct ChatOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation: Option<crate::data::automation_records::CommandAutomationResult>,
    /// The last accepted turn's native-confirmed answer (automation only).
    #[serde(skip)]
    pub automation_final_message: Option<String>,
    pub agent: Option<String>,
    pub exit_code: Option<i32>,
}

pub trait ChatCommandFrontend:
    UserMessageSink
    + crate::command::commands::agent_setup::AgentLaunchFrontend
    + crate::engine::acp::AcpFrontend
    + Send
    + Sync
{
    fn report_session_worktree(&mut self, _path: &std::path::Path, _branch: Option<&str>) {}

    /// The resolved native start mode, mount access and transport of the
    /// launch (WI 0070). Called once, after the agent options resolve.
    /// Presentation only: the decision is already made.
    fn report_agent_policy(
        &mut self,
        _policy: crate::data::agent_policy::ResolvedAgentPolicy,
        _step: Option<&str>,
    ) {
    }
}

pub struct ChatCommand {
    profile_request: super::launch_policy::settings_profile::ProfileRequest,
    flags: ChatCommandFlags,
    engines: Engines,
    session: Session,
    /// `--read-only-workspace` (WI 0070); a read-only lease engine adds to it.
    workspace_access: crate::data::workspace::WorkspaceAccess,
    /// The command's cancellation (daemon cancel, foreground interrupt).
    cancel: tokio_util::sync::CancellationToken,
}

impl ChatCommand {
    pub fn new(flags: ChatCommandFlags, engines: Engines, session: Session) -> Self {
        Self {
            cancel: Default::default(),
            profile_request: Default::default(),
            flags,
            engines,
            session,
            workspace_access: Default::default(),
        }
    }

    /// Mount the workspace read-only for this chat (`--read-only-workspace`).
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

    pub fn with_profile(
        mut self,
        request: super::launch_policy::settings_profile::ProfileRequest,
    ) -> Self {
        self.profile_request = request;
        self
    }

    pub fn with_cancellation(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn flags(&self) -> &ChatCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ChatCommand {
    type Frontend = Box<dyn ChatCommandFrontend>;
    type Outcome = ChatOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        // 0. Read-only workspace (WI 0070): refused on a runtime that cannot
        //    enforce it, or with --allow-docker, before any worktree, image or
        //    container work. The full policy is resolved once below.
        let requested_access = self.workspace_access;
        if let Err(e) = super::workspace_access::preflight_workspace_access(
            super::workspace_access::effective_access(requested_access, &self.engines.agent_engine),
            self.flags.allow_docker,
            self.engines.runtime.as_ref(),
        ) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("chat: {e}"),
            });
            return Err(e);
        }
        // 0b. Automation admission (WI 0070) before any preparation.
        crate::command::automation::launch::validate_request(
            &["chat"],
            self.flags.automation,
            &self.profile_request,
            self.flags.non_interactive,
        )?;
        // 1. Resolve the agent: --agent flag wins over the repo / global default.
        let session = self.session;
        let agent = match LaunchPolicy::for_session(&session).resolve_agent(&self.flags.agent) {
            Ok(a) => a,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("chat: failed to resolve agent: {e}"),
                });
                return Err(e);
            }
        };
        // Review/approve a custom definition before anything resolves it.
        if let Err(e) = ensure_agent_trusted(&session, &agent, frontend.as_mut()) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("chat: {e}"),
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
                    worktree.path.clone(),
                    worktree.path.clone(),
                    &invoker,
                )?,
                None => invoker.clone(),
            };
            let image_session = match &worktree {
                Some(worktree) => Session::open_worktree_with_invoker_config(
                    worktree.invoker_root.clone(),
                    worktree.invoker_root.clone(),
                    &invoker,
                )?,
                None => invoker.clone(),
            };

            // Launch mode is independent of agent resolution.  Resolve it before
            // mount/overlay/setup work so an unsupported ACP request cannot touch
            // the container path.
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
            // before setup, credentials or mounts (WI 0070).
            let agent_policy = LaunchPolicy::for_session(&session).resolve_agent_policy(
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
                    requested_access,
                    launch: launch_decision,
                    allow_docker: self.flags.allow_docker,
                    non_interactive: self.flags.non_interactive,
                    model_requested: self.flags.model.is_some(),
                },
                &self.engines.agent_engine,
            );
            let agent_policy = match agent_policy {
                Ok(outcome) => outcome,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("chat: {e}"),
                    });
                    return Err(e);
                }
            };
            for notice in &agent_policy.notices {
                frontend.write_message(notice.clone());
            }
            let workspace_access = agent_policy.policy.workspace_access;
            let launch_engine = super::workspace_access::agent_engine_for(
                workspace_access,
                &self.engines.agent_engine,
            );

            if let Some(note) = LaunchPolicy::deprecation_warning(&agent) {
                frontend.write_message(note);
            }

            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("chat: using agent '{}'", agent.as_str()),
            });

            if worktree.is_none() {
                // 1b. Confirm mount scope when cwd differs from git root.
                let cwd = session.working_dir().to_path_buf();
                let _mount_path =
                    match MountScope::resolve(&cwd, session.git_root(), frontend.as_mut()) {
                        Ok(p) => p,
                        Err(e) => {
                            frontend.write_message(UserMessage {
                                level: MessageLevel::Error,
                                text: format!("chat: mount scope resolution failed: {e}"),
                            });
                            return Err(e);
                        }
                    };
            }

            // 2. Parse overlay specs before PTY is activated so errors surface early.
            let cli_typed = match LaunchPolicy::parse_overlay_flags(&self.flags.overlay) {
                Ok(overlays) => overlays,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("chat: invalid overlay spec: {e}"),
                    });
                    return Err(e);
                }
            };
            let collected = session
                .effective_config()
                .collected_overlays(cli_typed, None, None)?;

            // Emit deprecation warnings for legacy config fields.
            LaunchPolicy::for_session(&session).warn_config(frontend.as_mut());

            let profile_run=AgentRunOptions {non_interactive:self.flags.non_interactive,model:self.flags.model.clone(),agent_flags:self.flags.agent_flags.clone(),launch_mode:if launch_decision==LaunchModeDecision::Acp {crate::data::config::agents::LaunchMode::Acp}else{crate::data::config::agents::LaunchMode::Stdio},..Default::default()};
            let definition=launch_engine.resolve_definition(&session,&agent)?;
            if self.flags.automation.is_some() {
                crate::command::automation::launch::require_adapter(&["chat"], &definition)?;
                if launch_decision == LaunchModeDecision::Acp {
                    return Err(CommandError::NativeCapabilityUnavailable(
                        "chat --automation uses the native terminal route, not ACP".into(),
                    ));
                }
            }
            profile=super::launch_policy::settings_profile::enter(&session,&agent,&self.profile_request,&profile_run,&definition,self.engines.runtime.clone()).await?;
            // 3. Ensure the agent is available. The Dockerfile + image setup is
            //    container-paradigm only; kit-declarative (sandbox) runtimes get
            //    their per-agent kits from `awman ready`, and a missing kit
            //    surfaces as a clear error at launch. Runs before PTY activation
            //    so any download/build progress streams to the user terminal.
            if self.engines.runtime.capabilities().kit_declarative {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: format!(
                        "chat: {} runtime active — using the agent kit prepared by `awman ready` \
                     (no image build needed)",
                        self.engines.runtime.display_name()
                    ),
                });
            } else {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Info,
                    text: "Checking agent availability…".into(),
                });
                match ensure_agent_setup(
                    self.engines.agent_engine.as_ref(),
                    &image_session,
                    &agent,
                    &mut frontend,
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("chat: agent setup failed: {e}"),
                        });
                        return Err(e);
                    }
                }
            }

            // 4. Resolve agent authentication (keychain credentials) and inject
            //    them as container env-vars so the running agent can reach its
            //    backend.
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
                        text: format!("chat: credential resolution failed: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };
            // File delivery is container-only. Keep sbx on its legacy env-only
            // keychain path without changing dsbx itself.
            let credentials = if self.engines.runtime.capabilities().kit_declarative
                && matches!(
                    resolved_credentials.delivery,
                    crate::engine::auth::CredentialDelivery::File(_)
                ) {
                self.engines.auth_engine.agent_env_credentials(&agent)?
            } else {
                resolved_credentials
            };

            // 5. Resolve context overlays.
            let (context_overlays, system_prompt) = LaunchPolicy::for_session(&session)
                .with_git(&self.engines.git_engine)
                .resolve_context_overlays(
                    &collected.context_overlays,
                    &agent,
                    None,
                    None,
                    frontend.as_mut(),
                )?;

            // The read-only section goes last (WI 0070).
            let system_prompt = super::workspace_access::system_prompt_for(
                workspace_access,
                system_prompt,
                false,
                &context_overlays,
            );

            // 6. Build the run options from flags + credentials. yolo/auto
            //    are the resolved values: --plan suppresses inherited ones.
            let mut run_opts = AgentRunOptions {
                structured_output: profile.is_some(),
                native_session: self.profile_request.agent_session.clone(),
                profile_state: profile.as_ref().map(super::launch_policy::settings_profile::ProfileSession::run_state),
                command_secrets: self.engines.command_secrets(),
                yolo: agent_policy.yolo.then_some(YoloMode::Enabled),
                yolo_authorized: self.flags.yolo_authorized,
                auto: agent_policy.auto.then_some(AutoMode::Enabled),
                plan: self.flags.plan.then_some(PlanMode::Enabled),
                allow_docker: self.flags.allow_docker,
                non_interactive: self.flags.non_interactive,
                model: self.flags.model.clone(),
                agent_flags: self.flags.agent_flags.clone(),
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
                    LaunchModeDecision::Acp => crate::data::config::agents::LaunchMode::Acp,
                    _ => crate::data::config::agents::LaunchMode::Stdio,
                },
                ..Default::default()
            };

            // 6. Build the paradigm-appropriate options through AgentEngine's
            //    centralized cross-paradigm mapper (container vs sandbox), folding in
            //    resolved credentials.
            if let Some(worktree) = &worktree {
                if !self.engines.runtime.capabilities().arbitrary_host_mounts {
                    return Err(CommandError::Other(
                        "This runtime cannot mount the linked worktree's required Git directory"
                            .into(),
                    ));
                }
                run_opts.image_tag_override = Some(crate::data::image_tags::agent_image_tag(
                    &worktree.invoker_root,
                    agent.as_str(),
                ));
                super::session_worktree::anchor_relative_overlays(
                    &mut run_opts.directory_overlays,
                    invoker.working_dir(),
                )?;
                if let Some(mut overlay) = worktree
                    .git_directory_overlay(self.engines.git_engine.clone())
                    .await?
                {
                    if workspace_access.is_read_only() {
                        // The worktree's Git directory is part of the
                        // workspace: a read-only chat must not commit or
                        // rewrite refs there.
                        super::workspace_access::restrict_to_access(&mut overlay, workspace_access);
                        run_opts.command_mounts.push(overlay);
                    } else {
                        run_opts.directory_overlays.push(overlay);
                    }
                }
            }

            // WI 0070: an interactive automation launch mounts its
            // execution-private native lifecycle channel.
            let automation_channel = match profile.as_ref() {
                Some(profile) if self.flags.automation.is_some() => crate::command::automation::launch::prepare_channel(
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
                        text: format!("chat: failed to build agent options: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };

            for note in LaunchPolicy::launch_notices(&agent, &run_opts) {
                frontend.write_message(note);
            }
            if workspace_access.is_read_only() {
                frontend.write_message(super::workspace_access::read_only_message(None));
            }
            if let Some(message) =
                LaunchPolicy::native_mode_message(&agent, &agent_policy.policy, None)
            {
                frontend.write_message(message);
            }
            frontend.report_agent_policy(agent_policy.policy, None);

            // 7. Build the agent instance.
            let instance = match self.engines.runtime.build(resolved) {
                Ok(i) => i,
                Err(e) => {
                    frontend.write_message(UserMessage {
                        level: MessageLevel::Error,
                        text: format!("chat: failed to build agent instance: {e}"),
                    });
                    return Err(CommandError::from(e));
                }
            };

            if let Some(profile)=profile.as_mut() {
                let redaction = instance.output_redaction();
                profile.bind_redaction(redaction)?;
                profile.bind_resource(&instance.handle_preview().name)?;
            }
            // 8. Run with PTY-active gating.
            frontend.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!("Launching agent ({})…", self.engines.runtime.display_name()),
            });
            let exit = if let Some(mode) = self.flags.automation {
                let run = crate::command::automation::launch::start(
                    crate::command::automation::launch::StartRequest {
                        command: &["chat"],
                        session: &session,
                        definition: &definition,
                        profile: profile.as_ref().expect("automation requires a settings profile"),
                        policy: &agent_policy.policy,
                        mode,
                        requested_native: self.profile_request.agent_session.clone(),
                        runtime: self.engines.runtime.runtime_name().to_string(),
                        host: frontend.automation_host(),
                        initial_task: None,
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
                automation_finished = Some(finished);
                exit
            } else if launch_decision == LaunchModeDecision::Acp {
                let (runtime_frontend, transport) = crate::engine::acp::AcpTransport::channel();
                let execution = match instance.run_with_frontend(Box::new(runtime_frontend)) {
                    Ok(execution) => execution,
                    Err(e) => {
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("chat: failed to launch ACP agent: {e}"),
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
                )
                .with_agent_name(agent.as_str());
                if let Err(e) = acp
                    .initialize("/workspace", agent_policy.acp_mode_id.as_deref())
                    .await
                {
                    // Reap the launched container before returning so it is not left
                    // running after a failed handshake.
                    let _ = acp.shutdown().await;
                    return Err(CommandError::from(e));
                }
                acp.drive(frontend.as_mut()).await
            } else {
                frontend.set_pty_active(true);
                let mut container_frontend = frontend.container_frontend_for_pty();
                if profile.is_some() && run_opts.non_interactive {
                    observe_native=true;
                    container_frontend=crate::engine::agent_output::native_observation::with_native_observation(container_frontend,definition.output_format,native_stream.clone());
                }
                let mut execution = match instance.run_with_frontend(container_frontend) {
                    Ok(e) => e,
                    Err(e) => {
                        frontend.set_pty_active(false);
                        frontend.replay_queued();
                        frontend.write_message(UserMessage {
                            level: MessageLevel::Error,
                            text: format!("chat: failed to launch agent: {e}"),
                        });
                        return Err(CommandError::from(e));
                    }
                };
                frontend.set_stuck_sender(execution.stuck_sender());
                let handle=execution.handle().clone();
                let exit=tokio::select! {
                    result=execution.wait()=>result,
                    _=native_stream.mismatch.cancelled()=>{
                        let _=self.engines.runtime.stop(&handle);
                        let _=execution.wait().await;
                        Err(crate::engine::error::EngineError::Config("provider did not resume the requested conversation".into()))
                    }
                };
                frontend.set_pty_active(false);
                frontend.wait_output_drained().await;
                frontend.replay_queued();
                exit
            };

            LaunchPolicy::report_session_end(frontend.as_mut(), "chat", &exit);

            if let Some(finished) = &automation_finished {
                crate::command::automation::launch::into_command_result(finished)?;
            }
            let exit_code = crate::command::dispatch::outcomes::agent_exit_code(exit, false)?;
            Ok(ChatOutcome {
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
            {
                if let Err(error) = profile.observe_native_session(id) {
                    outcome = Err(error);
                }
            }
            let successful_exit = outcome
                .as_ref()
                .is_ok_and(|result| result.exit_code.unwrap_or_default() == 0);
            let turn =
                profile_turn_state(successful_exit, native_stream.complete(), observe_native);
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

fn profile_turn_state(
    successful_exit: bool,
    native_turn_completed: bool,
    observes_native_completion: bool,
) -> crate::data::fs::agent_profiles::TurnStateAtExit {
    use crate::data::fs::agent_profiles::TurnStateAtExit::*;
    if !successful_exit {
        Interrupted
    } else if native_turn_completed || !observes_native_completion {
        Complete
    } else {
        Unknown
    }
}

pub(crate) async fn ensure_agent_setup(
    agent_engine: &crate::engine::agent::AgentEngine,
    session: &Session,
    agent: &AgentName,
    frontend: &mut Box<dyn ChatCommandFrontend>,
) -> Result<(), CommandError> {
    let mut adapter =
        crate::command::commands::agent_setup::AgentFrontendAdapter::new(frontend.as_mut());
    agent_engine
        .ensure_available_for_session(session, agent, &mut adapter)
        .await
        .map_err(CommandError::from)
}

#[cfg(test)]
mod profile_turn_tests {
    use super::profile_turn_state;
    use crate::data::fs::agent_profiles::TurnStateAtExit;

    #[test]
    fn successful_interactive_exit_is_resumable_but_incomplete_headless_is_unknown() {
        assert!(matches!(
            profile_turn_state(true, false, false),
            TurnStateAtExit::Complete
        ));
        assert!(matches!(
            profile_turn_state(true, false, true),
            TurnStateAtExit::Unknown
        ));
        assert!(matches!(
            profile_turn_state(false, false, false),
            TurnStateAtExit::Interrupted
        ));
    }
}
