//! `ReadyCommand` — thin wrapper over `ReadyEngine`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::launch_policy::{ensure_agent_trusted, LaunchPolicy};
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::command::headless::HeadlessDefaults;
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::preparation::{AuthenticationCheck, PreparationPolicy};
use crate::data::ready_image::{ReadyBaseImageResult, ReadyImageResult};
use crate::data::ready_summary::AgentCredentialHealth;
use crate::data::step_status::StepStatus;
use crate::engine::ready::{ReadyEngine, ReadyEngineOptions, ReadyFrontend, ReadySummary};

mod source_setup;
#[cfg(test)]
mod source_setup_contracts;

#[derive(Debug, Clone)]
pub struct ReadyCommandFlags {
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub non_interactive: bool,
    pub allow_docker: bool,
    pub json: bool,
    /// `--prepare-only`: image preparation without host-agent pings, credential
    /// checks or audits.
    pub prepare_only: bool,
    /// `--agent`; `None` readies the configured agent.
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadyOutcome {
    pub runtime: String,
    pub dockerfile: StepStatus,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_source: Option<StepStatus>,
    /// The custom-definition step (`None` for a first-party agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_definition: Option<StepStatus>,
    pub local_agent: StepStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_auth: Vec<(String, StepStatus)>,
    pub audit: StepStatus,
    pub image_rebuild: StepStatus,
    pub agent_credentials: Vec<AgentCredentialHealth>,
    /// Per non-default agent image status.
    pub non_default_agent_images: Vec<(String, StepStatus)>,
    /// Per-image build and import results, for runtimes that import images
    /// (builtin). Empty on build runtimes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_results: Vec<ReadyImageResult>,
    /// Project base images built into source stores, one per store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_image_results: Vec<ReadyBaseImageResult>,
    /// Whether every required image is prepared, from
    /// [`ReadySummary::is_ready`]: no image stage failed and no per-image or
    /// base-image stage was left pending or running. Drives the process/API
    /// exit code; the legacy JSON `ready` field also requires the host-side
    /// checks to have passed.
    pub ready: bool,
    /// Which policy this run used.
    pub preparation_policy: PreparationPolicy,
    /// Every requested non-authentication prerequisite succeeded (same
    /// reading as `ready`). Never implies authentication passed.
    pub images_ready: bool,
    /// What was established about provider authentication; `not_checked`
    /// under `images_only`.
    pub authentication: AuthenticationCheck,
    /// `true` when `--json` was passed; controls how the CLI renders the outcome.
    #[serde(skip)]
    pub json_requested: bool,
    /// `true` when `--refresh` was passed; carried into legacy JSON output.
    #[serde(skip)]
    pub refresh_requested: bool,
}

impl From<ReadySummary> for ReadyOutcome {
    fn from(s: ReadySummary) -> Self {
        let ready = s.is_ready();
        let preparation_policy = s.preparation_policy;
        let authentication = s.authentication();
        Self {
            runtime: s.runtime_name,
            dockerfile: s.dockerfile,
            base_image: s.base_image,
            agent_image: s.agent_image,
            image_source: s.image_source,
            agent_definition: s.agent_definition,
            local_agent: s.local_agent,
            agent_auth: s.agent_auth,
            audit: s.audit,
            image_rebuild: s.image_rebuild,
            agent_credentials: s.agent_credentials,
            non_default_agent_images: s.non_default_agent_images,
            image_results: s.image_results,
            base_image_results: s.base_image_results,
            ready,
            preparation_policy,
            images_ready: ready,
            authentication,
            json_requested: false,
            refresh_requested: false,
        }
    }
}

impl ReadyOutcome {
    /// Render the outcome in the legacy `awman ready --json` schema:
    ///
    /// ```json
    /// { "ready": <bool>,
    ///   "steps": {
    ///     "docker_daemon": {"status": "ok|skipped|failed|pending", "message": "..."},
    ///     "dockerfile": {...}, "aspec_folder": {...}, "work_items_config": {...},
    ///     "local_agent": {...}, "dev_image": {...}, "refresh": {...},
    ///     "image_rebuild": {...}
    ///   }
    /// }
    /// ```
    pub fn to_legacy_json(&self) -> serde_json::Value {
        fn step_to_json(s: &StepStatus) -> serde_json::Value {
            match s {
                StepStatus::Pending => serde_json::json!({"status": "pending", "message": ""}),
                StepStatus::Running => serde_json::json!({"status": "running", "message": ""}),
                StepStatus::Done => serde_json::json!({"status": "ok", "message": ""}),
                StepStatus::Skipped => serde_json::json!({"status": "skipped", "message": ""}),
                StepStatus::Warn(msg) => serde_json::json!({"status": "warn", "message": msg}),
                StepStatus::Failed(msg) => {
                    serde_json::json!({"status": "failed", "message": msg})
                }
            }
        }

        // `docker_daemon` isn't tracked as a separate step in the new engine;
        // if we made it this far, the daemon was reachable.
        let docker_daemon = StepStatus::Done;

        // `aspec_folder` and `work_items_config` are owned by `init`, not by
        // `ready`. Report them as Skipped so consumers see a complete schema.
        let aspec_folder = StepStatus::Skipped;
        let work_items_config = StepStatus::Skipped;

        // `refresh` is derived from the flag — Done if user asked for it,
        // Skipped otherwise.
        let refresh = if self.refresh_requested {
            StepStatus::Done
        } else {
            StepStatus::Skipped
        };

        // `ready` keeps its broad legacy reading: every required image stage
        // succeeded (`self.ready`) and no host-side check failed either. The
        // exit status uses the narrower `self.ready` alone.
        let failed = |s: &StepStatus| matches!(s, StepStatus::Failed(_));
        let checks_passed = self.ready
            && ![&self.dockerfile, &self.local_agent, &self.image_rebuild]
                .into_iter()
                .any(failed)
            && !self.non_default_agent_images.iter().any(|(_, s)| failed(s));

        let mut result = serde_json::json!({
            "ready": checks_passed,
            "preparation_policy": self.preparation_policy.as_str(),
            "images_ready": self.images_ready,
            "authentication": self.authentication.as_str(),
            "runtime": self.runtime,
            "agent_credentials": self.agent_credentials,
            "agent_auth": self.agent_auth,
            "steps": {
                "docker_daemon": step_to_json(&docker_daemon),
                "dockerfile": step_to_json(&self.dockerfile),
                "aspec_folder": step_to_json(&aspec_folder),
                "work_items_config": step_to_json(&work_items_config),
                "local_agent": step_to_json(&self.local_agent),
                "dev_image": step_to_json(&self.agent_image),
                "refresh": step_to_json(&refresh),
                "image_rebuild": step_to_json(&self.image_rebuild),
            }
        });
        if let Some(source) = &self.image_source {
            result["steps"]["image_source"] = step_to_json(source);
        }
        // Separate per-image build/import stages and base-image builds, for
        // runtimes that import images (builtin). Additive: absent entirely
        // on build runtimes, which never populate them.
        if !self.image_results.is_empty() {
            result["image_results"] =
                serde_json::to_value(&self.image_results).unwrap_or(serde_json::Value::Null);
            let warnings: Vec<_> = self
                .image_results
                .iter()
                .flat_map(|r| r.warnings.iter())
                .collect();
            if !warnings.is_empty() {
                result["warnings"] =
                    serde_json::to_value(&warnings).unwrap_or(serde_json::Value::Null);
            }
        }
        if !self.base_image_results.is_empty() {
            result["base_image_results"] =
                serde_json::to_value(&self.base_image_results).unwrap_or(serde_json::Value::Null);
        }
        result
    }
}

/// `ready` asks the trust question before resolving a custom agent.
pub trait ReadyCommandFrontend:
    ReadyFrontend + crate::command::commands::agent_trust::AgentTrustFrontend + Send
{
}
impl<T: ReadyFrontend + crate::command::commands::agent_trust::AgentTrustFrontend + Send>
    ReadyCommandFrontend for T
{
}

pub struct ReadyCommand {
    flags: ReadyCommandFlags,
    engines: Engines,
    session: crate::data::session::Session,
    headless: Option<HeadlessDefaults>,
    /// The caller is a local user's own CLI/TUI session, so a person may be
    /// asked to choose an image source. `false` unless `from_input` saw a
    /// local frontend (fail closed).
    local_user: bool,
    #[cfg(test)]
    host_ping: Option<std::sync::Arc<dyn crate::engine::ready::ReadyHostPing>>,
}

impl ReadyCommand {
    pub fn new(
        flags: ReadyCommandFlags,
        engines: Engines,
        session: crate::data::session::Session,
    ) -> Self {
        Self {
            flags,
            engines,
            session,
            headless: None,
            local_user: false,
            #[cfg(test)]
            host_ping: None,
        }
    }

    /// Inject a fixed host check; component journeys never execute a host agent.
    #[cfg(test)]
    pub(crate) fn with_host_ping(
        mut self,
        ping: std::sync::Arc<dyn crate::engine::ready::ReadyHostPing>,
    ) -> Self {
        self.host_ping = Some(ping);
        self
    }

    /// Let a hand-built command ask for an image source, as `from_input`
    /// does for a local frontend.
    #[cfg(test)]
    pub(crate) fn with_local_user(mut self, local_user: bool) -> Self {
        self.local_user = local_user;
        self
    }

    /// Set the headless defaults `from_input` derives for an API caller.
    #[cfg(test)]
    pub(crate) fn with_headless(mut self, headless: Option<HeadlessDefaults>) -> Self {
        self.headless = headless;
        self
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10). `--json`
    /// declares `implies: ["headless"]`, so the implication is already
    /// applied in `flags` and is not re-derived here.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let flags = ReadyCommandFlags {
            refresh: ctx.flags.bool("refresh"),
            build: ctx.flags.bool("build"),
            no_cache: ctx.flags.bool("no-cache"),
            non_interactive: ctx.flags.bool("headless"),
            allow_docker: ctx.flags.bool("allow-docker"),
            json: ctx.flags.bool("json"),
            prepare_only: ctx.flags.bool("prepare-only"),
            agent: ctx.flags.string("agent"),
        };
        // `--refresh` runs an agent audit; preparation-only runs no agent.
        // The catalogue declares the conflict; this keeps every route that
        // builds the command directly honest too.
        if flags.prepare_only && flags.refresh {
            return Err(CommandError::InvalidFlagValue {
                command: vec!["ready".into()],
                flag: "prepare-only".into(),
                reason: "--prepare-only runs no agent audit and cannot be combined with --refresh"
                    .into(),
            });
        }
        if flags.refresh && Self::headless_for(ctx.caller.frontend()).is_some() {
            return Err(CommandError::InvalidFlagValue {
                command: vec!["ready".into()],
                flag: "refresh".into(),
                reason: "--refresh is unavailable through the API because its audit runs an agent outside a command".into(),
            });
        }
        let mut command = Self::new(flags, ctx.engines.clone(), ctx.session.clone());
        command.headless = Self::headless_for(ctx.caller.frontend());
        command.local_user = ctx.caller.frontend().is_local_user();
        Ok(command)
    }

    /// API ready answers the two setup questions from the same Layer 2
    /// profile as API init. Local CLI/TUI invocations retain their frontend's
    /// interactive or piped-stdin behavior.
    pub fn headless_for(
        kind: crate::command::dispatch::catalogue::FrontendKind,
    ) -> Option<HeadlessDefaults> {
        match kind {
            crate::command::dispatch::catalogue::FrontendKind::Daemon { .. }
                if !kind.is_engine_local() =>
            {
                Some(HeadlessDefaults::api())
            }
            _ => None,
        }
    }

    pub fn flags(&self) -> &ReadyCommandFlags {
        &self.flags
    }
}

#[async_trait]
impl Command for ReadyCommand {
    type Frontend = Box<dyn ReadyCommandFrontend>;
    type Outcome = ReadyOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "ready: checking environment…".into(),
        });

        let mut session = self.session;
        // Resolve the agent from .awman/config.json (or global default), so
        // the per-agent Dockerfile lookup and image-tag computation match
        // whatever the user actually configured. Falls back to "claude" only
        // when no config sets the default. `--agent` wins over both.
        let agent = match LaunchPolicy::for_session(&session).resolve_agent(&self.flags.agent) {
            Ok(a) => a,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("ready: failed to resolve agent name: {e}"),
                });
                return Err(e);
            }
        };
        // A custom definition that needs approval is reviewed here, before
        // the engine resolves it (and refuses it, fail-closed, otherwise).
        if let Err(e) = ensure_agent_trusted(&session, &agent, frontend.as_mut()) {
            frontend.write_message(UserMessage {
                level: MessageLevel::Error,
                text: format!("ready: {e}"),
            });
            return Err(e);
        }

        let options = ReadyEngineOptions {
            agent: agent.clone(),
            refresh: self.flags.refresh,
            build: self.flags.build,
            no_cache: self.flags.no_cache,
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            env_passthrough: None,
            preparation: PreparationPolicy::from_prepare_only(self.flags.prepare_only),
        };

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "Checking {} availability…",
                self.engines.runtime.display_name()
            ),
        });

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Checking container runtime…".into(),
        });

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "Checking agent image…".into(),
        });

        // The active runtime, whatever its paradigm: ReadyEngine branches on
        // its capabilities itself (kit-declarative sandboxes emit a kit), and
        // refuses a custom agent on a runtime that cannot run one as a failed
        // step, before any kit or image work.
        let runtime = self.engines.runtime.clone();
        let resolved_builtin = resolve_builtin_inputs(
            &mut session,
            runtime.as_ref(),
            &agent,
            SourceSetupPolicy {
                flags: &self.flags,
                headless: self.headless.as_ref(),
                local_user: self.local_user,
            },
            frontend.as_mut(),
        )?;
        let mut engine = ReadyEngine::new(
            resolved_builtin,
            crate::data::fs::data_paths::data_home(session.effective_config().env())?,
            std::sync::Arc::new(session),
            self.engines.git_engine.clone(),
            self.engines.overlay_engine.clone(),
            runtime,
            self.engines.agent_engine.clone(),
            options,
        );
        #[cfg(test)]
        if let Some(ping) = self.host_ping {
            engine.set_host_ping(ping);
        }
        // Preparation-only does not look at provider credentials either.
        if !self.flags.prepare_only {
            engine.set_agent_credentials(credential_health(&self.engines.auth_engine, &agent));
        }
        let cancel = engine.cancel_token();
        let mut answering = HeadlessReadyFrontend {
            inner: frontend.as_mut(),
            defaults: self.headless.clone(),
        };
        let result = {
            let run = engine.run_to_completion(&mut answering);
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => result,
                _ = tokio::signal::ctrl_c() => {
                    cancel.cancel();
                    run.await
                }
            }
        };
        let summary = match result {
            Ok(s) => s,
            Err(e) => {
                // Acquisition owns bounded retries against the selected source.
                // Never restart ready or reopen source setup on a final failure.
                return Err(finish_ready_error(e));
            }
        };
        frontend.replay_queued();
        let mut outcome: ReadyOutcome = summary.into();
        outcome.json_requested = self.flags.json;
        outcome.refresh_requested = self.flags.refresh;

        Ok(outcome)
    }
}

/// Who may be asked for an image source in this invocation.
struct SourceSetupPolicy<'a> {
    flags: &'a ReadyCommandFlags,
    headless: Option<&'a HeadlessDefaults>,
    local_user: bool,
}

/// The builtin inputs the engine is built from. On an importing runtime, and
/// only when this invocation may ask a person, an image without a source is
/// offered once; after a saved choice the session is reloaded and the inputs
/// are resolved again, so the same `ready` run continues with the new source.
/// An invalid configured `builtin` block fails here, before any offer.
fn resolve_builtin_inputs(
    session: &mut crate::data::session::Session,
    runtime: &dyn crate::engine::agent_runtime::AgentRuntimeEngine,
    agent: &crate::data::session::AgentName,
    policy: SourceSetupPolicy<'_>,
    frontend: &mut dyn ReadyFrontend,
) -> Result<Option<crate::data::config::ResolvedBuiltinConfig>, CommandError> {
    let resolved = ReadyEngine::builtin_inputs(session, runtime)?;
    if resolved.is_none()
        || !source_setup::prompt_allowed(policy.flags, policy.headless, policy.local_user)
    {
        return Ok(resolved);
    }
    let installed = |tag: &str| runtime.image_exists(tag).unwrap_or(false);
    let setup = source_setup::offer_image_source(
        source_setup::SourceSetupContext {
            session,
            agent,
            build: policy.flags.build,
            installed: &installed,
        },
        frontend,
    )?;
    match setup {
        source_setup::SourceSetup::Saved { .. } => {
            Ok(ReadyEngine::builtin_inputs(session, runtime)?)
        }
        source_setup::SourceSetup::Unchanged => Ok(resolved),
    }
}

/// Applies API ready's Layer 2 answers while preserving build progress and
/// every other frontend report. A missing builtin image source remains the
/// engine's ordinary non-interactive configuration error; no source prompt
/// is introduced here.
struct HeadlessReadyFrontend<'a> {
    inner: &'a mut dyn ReadyCommandFrontend,
    defaults: Option<HeadlessDefaults>,
}

impl crate::data::message::UserMessageSink for HeadlessReadyFrontend<'_> {
    fn write_message(&mut self, message: UserMessage) {
        self.inner.write_message(message);
    }
    fn replay_queued(&mut self) {
        self.inner.replay_queued();
    }
}

impl crate::engine::agent::AgentImageFrontend for HeadlessReadyFrontend<'_> {
    fn report_step_status(
        &mut self,
        step: &crate::data::setup_step::SetupStep,
        status: StepStatus,
    ) {
        // The final summary owns diagnostics. Progress keeps only the status;
        // the outcome retains the full reasons for JSON and API consumers.
        let status = match status {
            StepStatus::Failed(_) => StepStatus::Failed(String::new()),
            StepStatus::Warn(_) => StepStatus::Warn(String::new()),
            other => other,
        };
        self.inner.report_step_status(step, status);
    }
    fn container_frontend(
        &mut self,
    ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
        self.inner.container_frontend()
    }
}

impl ReadyFrontend for HeadlessReadyFrontend<'_> {
    fn ask_create_dockerfile(
        &mut self,
        path: &std::path::Path,
    ) -> Result<bool, crate::engine::error::EngineError> {
        match &self.defaults {
            Some(defaults) => Ok(defaults.create_dockerfile()),
            None => self.inner.ask_create_dockerfile(path),
        }
    }
    fn ask_run_audit_on_template(&mut self) -> Result<bool, crate::engine::error::EngineError> {
        match &self.defaults {
            Some(defaults) => Ok(defaults.run_audit_on_template()),
            None => self.inner.ask_run_audit_on_template(),
        }
    }
    fn report_phase(&mut self, phase: &crate::data::ready_phase::ReadyPhase) {
        self.inner.report_phase(phase);
    }
    fn report_summary(&mut self, summary: &ReadySummary) {
        self.inner.report_summary(summary);
    }
    fn report_ping(&mut self, result: &crate::engine::ready::host_agent::LocalAgentPingResult) {
        self.inner.report_ping(result);
    }
}

impl crate::command::commands::agent_trust::AgentTrustFrontend for HeadlessReadyFrontend<'_> {}

fn credential_health(
    auth_engine: &crate::engine::auth::AuthEngine,
    agent: &crate::data::session::AgentName,
) -> Vec<AgentCredentialHealth> {
    let Ok(status) = auth_engine.list_agent_credentials(agent) else {
        return vec![AgentCredentialHealth {
            agent: agent.as_str().to_string(),
            refreshable: false,
            expires_in_secs: None,
            expired: false,
            read_error: Some("credential status could not be read".to_string()),
        }];
    };
    if !status.refreshable {
        return Vec::new();
    }
    let (expires_in_secs, expired) = match status.expires_at {
        Some(expires_at) => match expires_at.duration_since(std::time::SystemTime::now()) {
            Ok(remaining) => (
                Some(i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX)),
                false,
            ),
            Err(elapsed) => (
                Some(-i64::try_from(elapsed.duration().as_secs()).unwrap_or(i64::MAX)),
                true,
            ),
        },
        None => (None, false),
    };
    vec![AgentCredentialHealth {
        agent: status.agent.as_str().to_string(),
        refreshable: status.refreshable,
        expires_in_secs,
        expired,
        read_error: status.read_error.map(|error| error.to_string()),
    }]
}

/// Consume typed local-source finality before formatting the generic Io carrier.
/// Remote transient errors retain their type and acquisition's bounded retries.
/// There is no command-level retry, source-setup recovery or store fallback.
fn finish_ready_error(error: crate::engine::error::EngineError) -> CommandError {
    if crate::engine::oci::docker_engine::local_unavailability(&error).is_some() {
        return CommandError::Engine(crate::engine::error::EngineError::Other(
            crate::engine::oci::docker_engine::failure_message(&error),
        ));
    }
    error.into()
}
