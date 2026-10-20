//! `ReadyCommand` — thin wrapper over `ReadyEngine`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::message::{MessageLevel, UserMessage};
use crate::data::ready_image::{ReadyBaseImageResult, ReadyImageResult};
use crate::data::ready_summary::AgentCredentialHealth;
use crate::data::step_status::StepStatus;
use crate::engine::ready::{ReadyEngine, ReadyEngineOptions, ReadyFrontend, ReadySummary};

#[derive(Debug, Clone)]
pub struct ReadyCommandFlags {
    pub refresh: bool,
    pub build: bool,
    pub no_cache: bool,
    pub non_interactive: bool,
    pub allow_docker: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadyOutcome {
    pub runtime: String,
    pub dockerfile: StepStatus,
    pub base_image: StepStatus,
    pub agent_image: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_source: Option<StepStatus>,
    pub local_agent: StepStatus,
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
        Self {
            runtime: s.runtime_name,
            dockerfile: s.dockerfile,
            base_image: s.base_image,
            agent_image: s.agent_image,
            image_source: s.image_source,
            local_agent: s.local_agent,
            audit: s.audit,
            image_rebuild: s.image_rebuild,
            agent_credentials: s.agent_credentials,
            non_default_agent_images: s.non_default_agent_images,
            image_results: s.image_results,
            base_image_results: s.base_image_results,
            ready,
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
            "runtime": self.runtime,
            "agent_credentials": self.agent_credentials,
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

pub trait ReadyCommandFrontend: ReadyFrontend + Send {}
impl<T: ReadyFrontend + Send> ReadyCommandFrontend for T {}

pub struct ReadyCommand {
    flags: ReadyCommandFlags,
    engines: Engines,
    session: crate::data::session::Session,
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
        }
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10). `--json`
    /// declares `implies: ["non-interactive"]`, so the implication is already
    /// applied in `flags` and is not re-derived here.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        Ok(Self::new(
            ReadyCommandFlags {
                refresh: ctx.flags.bool("refresh"),
                build: ctx.flags.bool("build"),
                no_cache: ctx.flags.bool("no-cache"),
                non_interactive: ctx.flags.bool("non-interactive"),
                allow_docker: ctx.flags.bool("allow-docker"),
                json: ctx.flags.bool("json"),
            },
            ctx.engines.clone(),
            ctx.session.clone(),
        ))
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

        let session = self.session;
        // Resolve the agent from .awman/config.json (or global default), so
        // the per-agent Dockerfile lookup and image-tag computation match
        // whatever the user actually configured. Falls back to "claude" only
        // when no config sets the default.
        let agent = match LaunchPolicy::for_session(&session).resolve_agent(&None) {
            Ok(a) => a,
            Err(e) => {
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("ready: failed to resolve agent name: {e}"),
                });
                return Err(e);
            }
        };

        let options = ReadyEngineOptions {
            agent: agent.clone(),
            refresh: self.flags.refresh,
            build: self.flags.build,
            no_cache: self.flags.no_cache,
            allow_docker: self.flags.allow_docker,
            non_interactive: self.flags.non_interactive,
            env_passthrough: None,
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

        let runtime = self
            .engines
            .require_container_runtime()
            .map_err(CommandError::from)?
            .clone();
        let resolved_builtin = ReadyEngine::builtin_inputs(&session, runtime.as_ref())?;
        let mut engine = ReadyEngine::new(
            resolved_builtin,
            crate::data::config::global::GlobalConfig::data_home_with(
                session.effective_config().env(),
            )?,
            std::sync::Arc::new(session),
            self.engines.git_engine.clone(),
            self.engines.overlay_engine.clone(),
            runtime,
            self.engines.agent_engine.clone(),
            options,
        );
        engine.set_agent_credentials(credential_health(&self.engines.auth_engine, &agent));
        let cancel = engine.cancel_token();
        let result = {
            let run = engine.run_to_completion(frontend.as_mut());
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
                let cmd_err = CommandError::from(e);
                frontend.write_message(UserMessage {
                    level: MessageLevel::Error,
                    text: format!("ready: engine run failed: {cmd_err}"),
                });
                return Err(cmd_err);
            }
        };
        frontend.replay_queued();
        let mut outcome: ReadyOutcome = summary.into();
        outcome.json_requested = self.flags.json;
        outcome.refresh_requested = self.flags.refresh;

        frontend.write_message(UserMessage {
            level: MessageLevel::Info,
            text: format!(
                "ready: complete — runtime={}, dockerfile={:?}, base_image={:?}, agent_image={:?}, local_agent={:?}",
                outcome.runtime,
                outcome.dockerfile,
                outcome.base_image,
                outcome.agent_image,
                outcome.local_agent,
            ),
        });

        Ok(outcome)
    }
}

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

#[cfg(test)]
mod compat_tests {
    use super::*;
    use crate::data::config::image_source::ImageSourceKind;
    use crate::data::ready_image::ReadyImageWarning;

    #[test]
    fn builtin_image_source_is_visible_without_changing_docker_json() {
        let base = ReadySummary::new("docker");
        let docker = ReadyOutcome::from(base.clone()).to_legacy_json();
        assert!(docker["steps"].get("image_source").is_none());
        let mut builtin = base;
        builtin.image_source = Some(StepStatus::Done);
        let json = ReadyOutcome::from(builtin).to_legacy_json();
        assert_eq!(json["steps"]["image_source"]["status"], "ok");
    }

    fn image_result(build: StepStatus, import: StepStatus) -> ReadyImageResult {
        ReadyImageResult {
            agent: "claude".into(),
            tag: "awman-repo-claude:latest".into(),
            source_kind: Some(ImageSourceKind::DockerStore),
            reference: Some("awman-repo-claude:latest".into()),
            build,
            import,
            warnings: Vec::new(),
            optional: false,
        }
    }

    /// `From<ReadySummary>` no longer recomputes readiness from a handful of
    /// legacy fields: `ReadyOutcome::ready` is exactly
    /// `ReadySummary::is_ready()`, which a failed per-image build or import
    /// stage affects even though every legacy top-level field
    /// (`dockerfile`/`local_agent`/`image_rebuild`) stays `Done`.
    #[test]
    fn ready_outcome_ready_field_tracks_is_ready_not_just_legacy_fields() {
        let mut summary = ReadySummary::new("builtin");
        summary.base_image = StepStatus::Done;
        summary.agent_image = StepStatus::Done;
        summary.dockerfile = StepStatus::Done;
        summary.local_agent = StepStatus::Done;
        summary.image_rebuild = StepStatus::Skipped;
        summary
            .image_results
            .push(image_result(StepStatus::Done, StepStatus::Done));
        assert!(ReadyOutcome::from(summary.clone()).ready);

        summary.image_results[0].import = StepStatus::Failed("daemon unreachable".into());
        let outcome = ReadyOutcome::from(summary);
        assert!(
            !outcome.ready,
            "a failed per-image import must make the outcome not-ready even \
             though every legacy field is Done"
        );
    }

    /// `ReadyOutcome::from` copies `image_results`/`base_image_results`
    /// through unchanged — it does not drop or recompute them.
    #[test]
    fn ready_outcome_carries_image_and_base_image_results_through() {
        let mut summary = ReadySummary::new("builtin");
        summary
            .base_image_results
            .push(crate::data::ready_image::ReadyBaseImageResult {
                store: "docker unix:///var/run/docker.sock".into(),
                tag: "awman-repo:latest".into(),
                build: StepStatus::Done,
                optional: false,
            });
        let mut result = image_result(StepStatus::Done, StepStatus::Done);
        result
            .warnings
            .push(ReadyImageWarning::IncompatibleBuilder {
                tag: "awman-repo-codex:latest".into(),
                source_kind: ImageSourceKind::Archive,
                reason: "the configured source is not a local writable image store".into(),
            });
        summary.image_results.push(result);

        let outcome = ReadyOutcome::from(summary);
        assert_eq!(outcome.base_image_results.len(), 1);
        assert_eq!(outcome.image_results.len(), 1);
        assert_eq!(outcome.image_results[0].warnings.len(), 1);
    }

    /// Additive legacy-JSON fields: absent when there is nothing to report
    /// (an ordinary build-runtime `ready`, as today), present — with
    /// warnings flattened out of `image_results` — once a builder-enabled
    /// run recorded any.
    #[test]
    fn to_legacy_json_image_fields_are_additive() {
        let empty = ReadyOutcome::from(ReadySummary::new("docker")).to_legacy_json();
        assert!(empty.get("image_results").is_none());
        assert!(empty.get("base_image_results").is_none());
        assert!(empty.get("warnings").is_none());

        let mut summary = ReadySummary::new("builtin");
        let mut result = image_result(StepStatus::Done, StepStatus::Done);
        result
            .warnings
            .push(ReadyImageWarning::IncompatibleBuilder {
                tag: result.tag.clone(),
                source_kind: ImageSourceKind::Archive,
                reason: "the configured source is not a local writable image store".into(),
            });
        summary.image_results.push(result);
        let json = ReadyOutcome::from(summary).to_legacy_json();
        assert_eq!(json["image_results"].as_array().unwrap().len(), 1);
        assert_eq!(json["warnings"].as_array().unwrap().len(), 1);
        assert!(json.get("base_image_results").is_none());
    }

    /// The legacy `ready` field keeps its broad reading (M-7): it additionally
    /// requires `self.ready`, so a failed per-image stage now also turns the
    /// legacy `ready` false, even though no top-level legacy field failed.
    #[test]
    fn to_legacy_json_ready_requires_image_stages_too() {
        let mut outcome = ReadyOutcome::from({
            let mut summary = ReadySummary::new("builtin");
            summary.base_image = StepStatus::Done;
            summary.agent_image = StepStatus::Done;
            summary
                .image_results
                .push(image_result(StepStatus::Done, StepStatus::Done));
            summary
        });
        assert_eq!(outcome.to_legacy_json()["ready"], true);

        outcome.ready = false;
        assert_eq!(
            outcome.to_legacy_json()["ready"],
            false,
            "the legacy field must also go false once self.ready is false"
        );
    }
}
