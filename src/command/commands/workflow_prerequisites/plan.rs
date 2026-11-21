//! The complete, immutable scope of one workflow startup preflight.
//!
//! Static runs select their resolved step agents; dynamic runs select the
//! leader and every agent offered to it. Both feed the same engine probes,
//! report and preparation phase. Generated workflows may narrow this scope,
//! never expand it after the user has reviewed it.

use crate::command::commands::exec_workflow::{
    workflow_effective_config, ExecWorkflowCommandFlags,
};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::command::headless::HeadlessDefaults;
use crate::data::message::UserMessageSink;
use crate::data::session::Session;
use crate::data::workflow_definition::Workflow;
use crate::data::workflow_prerequisites::{
    PrerequisiteNeed, PrerequisiteStatus, WorkflowPrerequisite, WorkflowPrerequisiteReport,
    WorkflowPrerequisites,
};
use crate::engine::agent_runtime::ImageAcquisition;

pub(crate) struct WorkflowPreflightPlan {
    pub needs: WorkflowPrerequisites,
    default_agent: String,
}

impl WorkflowPreflightPlan {
    pub fn for_workflow(
        workflow: &Workflow,
        session: &Session,
        flags: &ExecWorkflowCommandFlags,
    ) -> Result<Self, CommandError> {
        let config = workflow_effective_config(session, flags)?;
        let default = config.agent();
        let mut agents = Vec::new();
        for step in &workflow.steps {
            let agent = step
                .agent
                .as_deref()
                .or(workflow.agent.as_deref())
                .or(default.as_deref())
                .ok_or_else(|| {
                    CommandError::Other(format!(
                        "workflow step '{}' resolves to no agent",
                        step.name
                    ))
                })?;
            agents.push(agent.to_owned());
        }
        // Phase remediation launches agents too. Match the engine's
        // on_failure precedence (explicit → workflow → claude).
        for remediation in workflow
            .setup
            .iter()
            .filter_map(|entry| entry.on_failure.as_ref())
            .chain(
                workflow
                    .teardown
                    .iter()
                    .filter_map(|entry| entry.on_failure.as_ref()),
            )
        {
            agents.push(
                remediation
                    .agent
                    .as_deref()
                    .or(workflow.agent.as_deref())
                    .unwrap_or("claude")
                    .to_owned(),
            );
        }
        if (!workflow.setup.is_empty() || !workflow.teardown.is_empty())
            && session.effective_config().base_image().is_none()
            && session.effective_config().runtime_selection()
                == Ok(crate::data::config::RuntimeSelection::BuiltinExperimental)
        {
            agents.push(session.configured_agent().as_str().to_owned());
        }
        Ok(Self::new(
            WorkflowPrerequisites::from_workflow(workflow),
            agents,
            default.unwrap_or_else(|| session.configured_agent().as_str().to_owned()),
            session,
        ))
    }

    pub fn dynamic(
        mut agents: Vec<String>,
        session: &Session,
        flags: &ExecWorkflowCommandFlags,
    ) -> Result<Self, CommandError> {
        let default_agent = workflow_effective_config(session, flags)?
            .agent()
            .unwrap_or_else(|| session.configured_agent().as_str().to_owned());
        if session.effective_config().base_image().is_none()
            && session.effective_config().runtime_selection()
                == Ok(crate::data::config::RuntimeSelection::BuiltinExperimental)
        {
            agents.push(session.configured_agent().as_str().to_owned());
        }
        Ok(Self::new(
            WorkflowPrerequisites::assume_all(),
            agents,
            default_agent,
            session,
        ))
    }

    fn new(
        mut needs: WorkflowPrerequisites,
        mut agents: Vec<String>,
        default_agent: String,
        session: &Session,
    ) -> Self {
        agents.sort();
        agents.dedup();
        for agent in agents {
            needs.merge_needs(PrerequisiteNeed {
                prerequisite: WorkflowPrerequisite::AgentImage {
                    tag: session.agent_image_tag(&agent),
                    agent,
                },
                needed_by: vec![],
            });
        }
        Self {
            needs,
            default_agent,
        }
    }

    /// Prepare missing and stale images advertised by the collected report.
    /// Interactive Continue authorizes these advertised actions. Unattended
    /// runs use the same agent-setup mode defaults as other launch commands.
    pub async fn prepare_images(
        &self,
        report: &mut WorkflowPrerequisiteReport,
        engines: &Engines,
        session: &Session,
        headless: Option<HeadlessDefaults>,
        sink: &mut dyn UserMessageSink,
    ) -> Result<(), CommandError> {
        use crate::command::commands::agent_setup::AgentSetupDecision;
        let paths = crate::data::RepoDockerfilePaths::new(session.git_root());
        // WI 0163: installed images whose inputs label no longer matches the
        // working tree are rebuilt before the run (warn-and-continue on a
        // failed rebuild); the missing ones are prepared below.
        let agents: Vec<String> = report
            .checks
            .iter()
            .filter_map(|check| match &check.prerequisite {
                WorkflowPrerequisite::AgentImage { agent, .. } => Some(agent.clone()),
                _ => None,
            })
            .collect();
        if !agents.is_empty() {
            crate::command::commands::workflow_preflight::refresh_stale_workflow_images(
                engines, session, &agents, sink, None,
            )
            .await?;
        }
        for check in &mut report.checks {
            let WorkflowPrerequisite::AgentImage { agent, tag } = &check.prerequisite else {
                continue;
            };
            let runtime = &engines.runtime;
            if runtime.capabilities().image_acquisition == ImageAcquisition::Kit {
                Self::record_image_readiness(check, engines, session);
                continue;
            }
            if runtime.capabilities().image_acquisition == ImageAcquisition::Build {
                let tag = session.project_image_tag();
                if !runtime.image_exists(&tag)? {
                    return Err(
                        crate::engine::error::EngineError::AgentRequiresProjectImage { tag }.into(),
                    );
                }
            }
            // A stale installed image is not a missing image. In particular,
            // headless missing-image policy must not block refresh or retry a
            // failed refresh that deliberately retained the old image.
            let present = runtime.image_exists(tag)?;
            // Importing runtimes must also validate a cached image's source
            // provenance against this invocation's effective configuration.
            if present && runtime.capabilities().image_acquisition != ImageAcquisition::Import {
                Self::record_image_readiness(check, engines, session);
                continue;
            }
            if !runtime.is_available() {
                return Err(CommandError::Other(
                    "workflow prerequisite: agent runtime is unavailable".into(),
                ));
            }
            if let Some(defaults) = headless.as_ref().filter(|_| {
                !present && runtime.capabilities().image_acquisition == ImageAcquisition::Build
            }) {
                let default_available = runtime
                    .image_exists(&session.agent_image_tag(&self.default_agent))
                    .unwrap_or(false);
                if defaults.agent_setup(default_available) != AgentSetupDecision::Setup {
                    return Err(CommandError::WorkflowPrerequisitesUnresolved {
                        report: Box::new(report.clone()),
                    });
                }
            }
            if runtime.capabilities().image_acquisition != ImageAcquisition::Kit {
                if let Err(error) =
                    crate::command::commands::workflow_preflight::ensure_agent_image(
                        engines,
                        session.git_root(),
                        &paths,
                        agent,
                        session,
                        sink,
                    )
                    .await
                {
                    check.status = PrerequisiteStatus::Missing;
                    check.explanation = error.to_string();
                    // Name the full config keys and files that give the image a
                    // source. Only when configuration has none: a configured
                    // source that failed keeps its own error. Never prompts.
                    if runtime.capabilities().image_acquisition == ImageAcquisition::Import {
                        let is_default = agent == session.configured_agent().as_str();
                        if let Some(guidance) =
                            crate::data::config::image_source_setup::missing_image_source_guidance(
                                session, tag, agent, is_default,
                            )
                        {
                            check.explanation = guidance;
                        }
                    }
                    return Err(CommandError::WorkflowPrerequisitesUnresolved {
                        report: Box::new(report.clone()),
                    });
                }
            }
            Self::record_image_readiness(check, engines, session);
        }
        Ok(())
    }

    fn record_image_readiness(
        check: &mut crate::data::workflow_prerequisites::PrerequisiteCheck,
        engines: &Engines,
        session: &Session,
    ) {
        let WorkflowPrerequisite::AgentImage { agent, tag } = &check.prerequisite else {
            return;
        };
        // Report the result of preparation, including warn-and-continue when
        // a refresh failed. Do not turn a retained stale image into success.
        match crate::engine::image_prep::workflow_image_preparation(
            engines.runtime.clone(),
            session,
            agent,
            tag,
        ) {
            Ok(None) => {
                check.status = PrerequisiteStatus::Satisfied;
                check.explanation = "image prepared for this workflow".into();
                check.remediations.clear();
            }
            Ok(Some(reason)) => {
                check.status = PrerequisiteStatus::Missing;
                check.explanation = format!("{reason}; continuing with the installed image");
                check.remediations =
                    vec![crate::data::workflow_prerequisites::Remediation::PrepareImage];
            }
            Err(error) => {
                check.status = PrerequisiteStatus::Unknown;
                check.explanation = format!("could not verify image {tag} after preparation: {error}; continuing with the installed image");
                check.remediations =
                    vec![crate::data::workflow_prerequisites::Remediation::PrepareImage];
            }
        }
    }
}

impl super::PreparedPrerequisites {
    /// Accept only the subset of agents and grants reviewed at startup.
    /// This is projection/validation, never another preflight or prompt.
    pub(crate) fn narrow_to(
        &mut self,
        workflow: &Workflow,
        session: &Session,
        flags: &ExecWorkflowCommandFlags,
        frontend: &mut dyn super::WorkflowPrerequisiteFrontend,
    ) -> Result<(), CommandError> {
        let concrete = WorkflowPreflightPlan::for_workflow(workflow, session, flags)?;
        let mut images = Vec::new();
        for need in &concrete.needs.needs {
            if let WorkflowPrerequisite::AgentImage { agent, .. } = &need.prerequisite {
                // Successful preparation guarantees availability, including
                // the explicitly reported stale-image fallback. Its report
                // must remain honest without rejecting a dynamic repair run.
                let check = self.report.find(&need.prerequisite)
                    .ok_or_else(|| CommandError::Other(format!(
                        "generated workflow selected agent '{agent}' outside the prepared workflow prerequisites"
                    )))?;
                images.push(check.clone());
            }
        }
        let (needs, mut report) = self.flow.narrow_after_generation(
            workflow,
            &self.report,
            &mut self.grants,
            &self.redaction,
            frontend,
        )?;
        report.checks.extend(images);
        self.remotes = self.flow.remotes_for(&needs);
        self.needs = needs;
        self.report = report;
        Ok(())
    }
}
