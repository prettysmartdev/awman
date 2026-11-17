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

    /// Prepare only images the single collected report found unavailable.
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
            let WorkflowPrerequisite::AgentImage { agent, .. } = &check.prerequisite else {
                continue;
            };
            let runtime = &engines.runtime;
            let present = check.status == PrerequisiteStatus::Satisfied;
            // Importing runtimes must also validate a cached image's source
            // provenance against this invocation's effective configuration.
            if present && runtime.capabilities().image_acquisition != ImageAcquisition::Import {
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
            if runtime.capabilities().image_acquisition == ImageAcquisition::Build {
                let tag = session.project_image_tag();
                if !runtime.image_exists(&tag)? {
                    return Err(
                        crate::engine::error::EngineError::AgentRequiresProjectImage { tag }.into(),
                    );
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
                    return Err(CommandError::WorkflowPrerequisitesUnresolved {
                        report: Box::new(report.clone()),
                    });
                }
            }
            check.status = PrerequisiteStatus::Satisfied;
            check.explanation = "image prepared for this workflow".into();
            check.remediations.clear();
        }
        Ok(())
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
                let check = self.report.find(&need.prerequisite)
                    .filter(|check| check.status == PrerequisiteStatus::Satisfied)
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
