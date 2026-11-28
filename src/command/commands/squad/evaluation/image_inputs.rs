//! Where a squad run's images come from, and preparing them once per run
//! (WI 0105) — Layer 2.
//!
//! A Git-backed task keeps building from its repository. Every
//! [`MountScope::Directory`] task — the default workspace and a custom
//! non-Git folder alike — builds from its task-owned image context, the
//! guarded `tasks/<name>/workspace/` folder that is also mounted at
//! `/awman/context/workflow`. A custom folder stays mounted only at
//! `/workspace` and is never a build context, never written, and never a
//! reason to look at its parent.
//!
//! [`prepare_task_images`] runs before the leader of each directory-task
//! run: migrate legacy recipes once, scaffold missing recipes, then let the
//! shared freshness pass compare the effective inputs with the installed
//! images' identities. Its result is pinned on the run's session
//! ([`PreparedImages::binding`]), so the leader, the generated workflow's
//! steps and its setup/teardown containers all run that one generation and
//! nothing refreshes or builds again mid-run. Recipe edits made during the
//! run are picked up by the next run's preparation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::command::commands::workflow_preflight::{prepare_workflow_images_at, BuildOutputTarget};
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::fs::squad_paths::{LegacyRecipePlan, SquadPaths, TaskImageContext};
use crate::data::fs::task_store::{MountScope, Task};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::{AgentName, Session};
use crate::data::worker::lease_images::LeaseImageBinding;
use crate::engine::agent::image_freshness::{MissingImages, SessionImageInputs};
use crate::engine::agent_runtime::ImageAcquisition;

/// Where one task's image recipes, build context and tag stem come from.
#[derive(Debug, Clone)]
pub enum ImageSource {
    /// Git-backed tasks: unchanged — the repository.
    Repository { git_root: PathBuf },
    /// Every `MountScope::Directory` task: the guarded task folder.
    TaskContext(TaskImageContext),
}

impl ImageSource {
    pub fn root(&self) -> &Path {
        match self {
            Self::Repository { git_root } => git_root,
            Self::TaskContext(context) => context.root(),
        }
    }

    pub fn task_context(&self) -> Option<&TaskImageContext> {
        match self {
            Self::TaskContext(context) => Some(context),
            Self::Repository { .. } => None,
        }
    }
}

/// The image source for `task`: decided by its captured mount scope alone,
/// never by its path, an enclosing repository or a parent directory.
pub fn image_source_for(
    task: &Task,
    paths: &SquadPaths,
    session: &Session,
) -> Result<ImageSource, CommandError> {
    match task.mount_scope {
        MountScope::Directory => Ok(ImageSource::TaskContext(
            paths
                .task_image_context(&task.name)
                .map_err(CommandError::Data)?,
        )),
        MountScope::GitRoot | MountScope::Cwd => Ok(ImageSource::Repository {
            git_root: session.git_root().to_path_buf(),
        }),
    }
}

/// How one image was made ready for this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedOutcome {
    /// The installed image's identity matched its inputs.
    Reused,
    /// Built (or rebuilt) by this preparation.
    Built,
    /// A refresh failed; the previously installed image is kept. Never fresh.
    StaleKept,
    /// An importing runtime acquired it through its configured source.
    Acquired,
}

/// One prepared image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImage {
    pub tag: String,
    /// The installed id, when the runtime reports one.
    pub image_id: Option<String>,
    pub outcome: PreparedOutcome,
}

/// Every image one directory-task run uses: one generation, prepared before
/// its leader starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImages {
    pub image_root: PathBuf,
    pub project: PreparedImage,
    pub agents: BTreeMap<String, PreparedImage>,
    pub warnings: Vec<crate::data::execution_event::ImageStaleWarning>,
}

impl PreparedImages {
    /// The tags pinned on the run's session. With it in place the shared
    /// launch and preflight paths use exactly these tags, refresh nothing
    /// and build nothing (the same contract a worker lease's prepared images
    /// have), so the run keeps one generation.
    pub fn binding(&self) -> LeaseImageBinding {
        LeaseImageBinding {
            project: self.project.tag.clone(),
            agents: self
                .agents
                .iter()
                .map(|(agent, image)| (agent.clone(), image.tag.clone()))
                .collect(),
        }
    }

    /// Fail with an actionable message when `agent` was not prepared.
    pub fn require(&self, agent: &str) -> Result<(), CommandError> {
        if self.agents.contains_key(agent) {
            return Ok(());
        }
        Err(CommandError::Other(format!(
            "agent '{agent}' has no image prepared for this run: add its recipe at {} \
             (or add it to squad.agentsToModels) and trigger the task again",
            crate::data::RepoDockerfilePaths::new(&self.image_root)
                .agent_dockerfile(agent)
                .display()
        )))
    }

    /// Whether any image was actually built by this preparation.
    pub fn built_any(&self) -> bool {
        std::iter::once(&self.project)
            .chain(self.agents.values())
            .any(|image| image.outcome == PreparedOutcome::Built)
    }
}

/// Move a custom non-Git task's legacy recipes into its task context, once.
///
/// Skipped for a default task (its working tree *is* the context) and once
/// the marker exists. A refused plan fails the run with the planner's
/// actionable message before anything is built or written.
pub fn migrate_legacy_recipes(
    task: &Task,
    context: &TaskImageContext,
    agents: &[String],
    sink: &mut dyn UserMessageSink,
) -> Result<(), CommandError> {
    let legacy = task.repo_scope.as_path();
    let same = std::fs::canonicalize(legacy).ok() == std::fs::canonicalize(context.root()).ok();
    if same || context.load_state().map_err(CommandError::Data)?.is_some() {
        return Ok(());
    }
    let plan = context
        .plan_legacy_migration(legacy, agents)
        .map_err(CommandError::Data)?;
    if let Some(message) = plan.refusal_message(&task.name, legacy, context.root()) {
        return Err(CommandError::Other(message));
    }
    context
        .apply_legacy_migration(legacy, &plan)
        .map_err(CommandError::Data)?;
    if let LegacyRecipePlan::Copy { files, kept } = &plan {
        for file in files {
            sink.write_message(UserMessage {
                level: MessageLevel::Info,
                text: format!(
                    "moved {} from {} into the task image context {}",
                    file.rel,
                    legacy.display(),
                    context.root().display()
                ),
            });
        }
        for rel in kept {
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: format!(
                    "task-owned {rel} in {} kept; {rel} in {} is no longer used",
                    context.root().display(),
                    legacy.display()
                ),
            });
        }
    }
    context.record_state(legacy).map_err(CommandError::Data)?;
    Ok(())
}

/// Prepare every image a directory-task run uses from its task context.
///
/// `agents` is the full set the leader may be offered. On a building
/// runtime the shared freshness pass decides each image ([`MissingImages::Build`]):
/// fresh → reused; stale or unlabeled → rebuilt, keeping the old image and
/// reporting `image_stale` when that fails; missing → built, failing the
/// run when that fails. An importing runtime acquires each agent through
/// its source as before. A runtime that runs no images (declarative kits)
/// prepares nothing and returns `None`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_task_images(
    engines: &Engines,
    session: &Session,
    task: &Task,
    context: &TaskImageContext,
    agents: &[String],
    sink: &mut dyn UserMessageSink,
    mut build_output: Option<&mut dyn BuildOutputTarget>,
) -> Result<Option<PreparedImages>, CommandError> {
    for name in agents {
        let agent = AgentName::new(name.clone()).map_err(CommandError::Data)?;
        engines.agent_engine.resolve_definition(session, &agent)?;
    }
    migrate_legacy_recipes(task, context, agents, sink)?;
    let warnings = crate::engine::squad::SquadAgentLauncher::ensure_task_image_context(
        session.env(),
        context,
        session.definition_root(),
        agents,
    )
    .await
    .map_err(CommandError::from)?;
    for warning in warnings {
        sink.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: warning,
        });
    }
    let root = context.root().to_path_buf();
    let dockerfiles = context.dockerfiles();
    // The full set the leader may be offered: the scaffolded agents plus
    // every other usable recipe found in the context.
    let mut with_recipe: Vec<String> = agents
        .iter()
        .filter(|agent| dockerfiles.agent_dockerfile(agent).is_file())
        .cloned()
        .collect();
    let (available, _) = engines
        .agent_engine
        .filter_available_agents(session, dockerfiles.discover_agent_dockerfiles());
    let mut discovered = Vec::new();
    for (agent, _) in available {
        if !with_recipe.contains(&agent) {
            with_recipe.push(agent.clone());
            discovered.push(agent);
        }
    }
    // A discovered agent is offered to the leader like a configured one, so
    // its recipe is synchronised with its approved definition (and a
    // divergent hand-owned body is warned about) before anything is hashed.
    if !discovered.is_empty() {
        for name in &discovered {
            let agent = AgentName::new(name.clone()).map_err(CommandError::Data)?;
            engines.agent_engine.resolve_definition(session, &agent)?;
        }
        let warnings = crate::engine::squad::SquadAgentLauncher::ensure_task_image_context(
            session.env(),
            context,
            session.definition_root(),
            &discovered,
        )
        .await
        .map_err(CommandError::from)?;
        for warning in warnings {
            sink.write_message(UserMessage {
                level: MessageLevel::Warning,
                text: warning,
            });
        }
    }
    context
        .verify_recipes(&with_recipe)
        .map_err(CommandError::Data)?;
    let project_tag = crate::data::image_tags::project_image_tag(&root);
    let runtime = engines.runtime.clone();
    match runtime.capabilities().image_acquisition {
        ImageAcquisition::Kit => Ok(None),
        ImageAcquisition::Import => {
            // The task's own `Dockerfile.dev` is the project recipe here too,
            // never `runtime.dockerfile` from session configuration.
            let project_dockerfile = dockerfiles.project_dockerfile();
            let mut prepared = BTreeMap::new();
            let mut warnings: Vec<crate::data::execution_event::ImageStaleWarning> = Vec::new();
            for agent in &with_recipe {
                let kept = crate::command::commands::workflow_preflight::import_task_agent_image(
                    engines,
                    &root,
                    &project_dockerfile,
                    agent,
                    session,
                    sink,
                    match build_output.as_mut() {
                        Some(output) => Some(&mut **output),
                        None => None,
                    },
                )
                .await?;
                let tag = crate::data::image_tags::agent_image_tag(&root, agent);
                let outcome = if kept.is_empty() {
                    PreparedOutcome::Acquired
                } else {
                    PreparedOutcome::StaleKept
                };
                for warning in kept {
                    if !warnings.iter().any(|w| w.image == warning.image) {
                        warnings.push(warning);
                    }
                }
                prepared.insert(
                    agent.clone(),
                    PreparedImage {
                        tag,
                        image_id: None,
                        outcome,
                    },
                );
            }
            let project_outcome = if warnings.iter().any(|w| w.image == project_tag) {
                PreparedOutcome::StaleKept
            } else {
                PreparedOutcome::Acquired
            };
            Ok(Some(PreparedImages {
                image_root: root,
                project: PreparedImage {
                    tag: project_tag,
                    image_id: None,
                    outcome: project_outcome,
                },
                agents: prepared,
                warnings,
            }))
        }
        ImageAcquisition::Build => {
            let inputs =
                SessionImageInputs::at(&root, &dockerfiles.project_dockerfile(), session.env())?;
            let refresh = prepare_workflow_images_at(
                engines,
                &inputs,
                &with_recipe,
                MissingImages::Build,
                sink,
                build_output,
            )
            .await?;
            let image = |tag: String| -> Result<PreparedImage, CommandError> {
                let image_id = match runtime.inspect_image(&tag) {
                    Ok(Some(image)) => Some(image.id),
                    Ok(None) => {
                        return Err(CommandError::Other(format!(
                            "image {tag} is missing after preparation"
                        )))
                    }
                    // A runtime without image metadata reports no id.
                    Err(
                        crate::engine::error::EngineError::UnsupportedOnRuntime { .. }
                        | crate::engine::error::EngineError::NotImplemented(_),
                    ) => None,
                    // A run is pinned to an established identity; an image
                    // that cannot be inspected is not "reused".
                    Err(error) => {
                        return Err(CommandError::Other(format!(
                            "could not verify image {tag} for this run ({error}); \
                             check that the container runtime is reachable and try again"
                        )))
                    }
                };
                let outcome = if refresh.warnings.iter().any(|w| w.image == tag) {
                    PreparedOutcome::StaleKept
                } else if refresh.rebuilt.contains(&tag) {
                    PreparedOutcome::Built
                } else {
                    PreparedOutcome::Reused
                };
                Ok(PreparedImage {
                    tag,
                    image_id,
                    outcome,
                })
            };
            let project = image(project_tag)?;
            let mut prepared = BTreeMap::new();
            for agent in &with_recipe {
                prepared.insert(
                    agent.clone(),
                    image(crate::data::image_tags::agent_image_tag(&root, agent))?,
                );
            }
            Ok(Some(PreparedImages {
                image_root: root,
                project,
                agents: prepared,
                warnings: refresh.warnings,
            }))
        }
    }
}
