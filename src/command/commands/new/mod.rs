//! `NewCommand` — `new spec`, `new workflow`, `new skill`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::launch_policy::LaunchPolicy;
use crate::command::commands::prompt_templates::{
    render_skill_interview_prompt, render_workflow_interview_prompt,
};
use crate::command::commands::skill_library::{
    pull_all_libraries, pull_library, resolve_pull_target, PullOutcome,
};
use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::fs::{SkillDirs, WorkflowDirs, SKILL_INTERVIEW_CONTAINER_DIR};
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::ContainerOption;

#[derive(Debug, Clone)]
pub struct NewSpecFlags {
    pub interview: bool,
    pub non_interactive: bool,
    pub issue_source: crate::engine::issue::IssueSourceFlags,
}

#[derive(Debug, Clone)]
pub struct NewWorkflowFlags {
    pub interview: bool,
    pub non_interactive: bool,
    pub global: bool,
    pub format: String,
}

#[derive(Debug, Clone)]
pub struct NewSkillFlags {
    pub interview: bool,
    pub non_interactive: bool,
    pub global: bool,
    pub pull: Option<String>,
    pub pull_all: bool,
    pub subdir: Option<String>,
}

#[derive(Debug, Clone)]
pub enum NewSubcommand {
    Spec(NewSpecFlags),
    Workflow(NewWorkflowFlags),
    Skill(NewSkillFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct NewSpecOutcome {
    pub interview: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewWorkflowOutcome {
    pub interview: bool,
    pub global: bool,
    pub format: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PullLibraryOutcome {
    pub slug: String,
    pub dir: String,
    pub updated: bool,
    pub skills_found: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NewSkillOutcome {
    pub interview: bool,
    pub global: bool,
    pub path: Option<String>,
    /// `true` when this run was a `--pull`/`--pull-all` library operation
    /// rather than skill creation. `libraries` alone cannot carry that
    /// distinction: `--pull-all` with nothing pulled yet also yields an empty
    /// list, and rendering it as a created skill would be wrong.
    pub pull: bool,
    pub libraries: Vec<PullLibraryOutcome>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum NewOutcome {
    Spec(NewSpecOutcome),
    Workflow(NewWorkflowOutcome),
    Skill(NewSkillOutcome),
}

/// A single step collected from the user during `new workflow`.
#[derive(Debug, Clone)]
pub struct WorkflowStepInput {
    pub name: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub prompt: String,
}

/// `NewCommandFrontend` extends `SpecsCommandFrontend` so the `Spec`
/// subcommand can drive the same Q&A (kind / title / summary).
pub trait NewCommandFrontend:
    UserMessageSink + crate::command::commands::specs::SpecsCommandFrontend + Send + Sync
{
    /// Prompt for a workflow name. CLI implementations gate on stdin TTY.
    fn ask_workflow_name(&mut self) -> Result<String, CommandError> {
        Ok("workflow".to_string())
    }
    /// Prompt for a human-readable workflow title.
    fn ask_workflow_title(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for a one-line summary for the new workflow (used in interview mode).
    fn ask_workflow_summary(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for a step name.
    fn ask_workflow_step_name(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for the optional agent override for the current step.
    fn ask_workflow_step_agent(&mut self) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    /// Prompt for the optional model override for the current step.
    fn ask_workflow_step_model(&mut self) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    /// Prompt for the step prompt text.
    fn ask_workflow_step_prompt(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Ask whether to add another workflow step.
    fn ask_add_another_step(&mut self) -> Result<bool, CommandError> {
        Ok(false)
    }
    /// Prompt for a skill name.
    fn ask_skill_name(&mut self) -> Result<String, CommandError> {
        Ok("skill".to_string())
    }
    /// Prompt for a one-line summary for the new skill (used in interview mode).
    fn ask_skill_summary(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
    /// Prompt for the body content of the new skill.
    fn ask_skill_body(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
    }
}

// ─── Serde structs for workflow serialization ────────────────────────────────

#[derive(Debug, Serialize)]
struct WorkflowFileToml<'a> {
    title: &'a str,
    #[serde(rename = "step", skip_serializing_if = "Vec::is_empty")]
    steps_toml: Vec<WorkflowStepSerde<'a>>,
}

#[derive(Debug, Serialize)]
struct WorkflowFileYaml<'a> {
    title: &'a str,
    steps: Vec<WorkflowStepSerde<'a>>,
}

#[derive(Debug, Serialize)]
struct WorkflowStepSerde<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    prompt: &'a str,
}

fn serialize_steps(steps: &[WorkflowStepInput]) -> Vec<WorkflowStepSerde<'_>> {
    steps
        .iter()
        .map(|s| WorkflowStepSerde {
            name: &s.name,
            agent: s.agent.as_deref(),
            model: s.model.as_deref(),
            prompt: &s.prompt,
        })
        .collect()
}

fn serialize_workflow_toml(title: &str, steps: &[WorkflowStepInput]) -> String {
    let file = WorkflowFileToml {
        title,
        steps_toml: serialize_steps(steps),
    };
    toml::to_string_pretty(&file).unwrap_or_else(|_| format!("title = \"{title}\"\n"))
}

fn serialize_workflow_yaml(title: &str, steps: &[WorkflowStepInput]) -> String {
    let file = WorkflowFileYaml {
        title,
        steps: serialize_steps(steps),
    };
    serde_yaml::to_string(&file).unwrap_or_else(|_| format!("title: \"{title}\"\nsteps: []\n"))
}

/// Subdirectory under the git root for user-authored workflow definitions.
const REPO_WORKFLOW_DEFINITIONS_DIR: &str = "aspec/workflows";

pub struct NewCommand {
    sub: NewSubcommand,
    engines: Engines,
    session: Session,
}

impl NewCommand {
    pub fn new(sub: NewSubcommand, engines: Engines, session: Session) -> Self {
        Self {
            sub,
            engines,
            session,
        }
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10). The three
    /// `new` leaves share one entry point, selected by the caller's canonical
    /// path; `--format` takes its `"toml"` from the catalogue.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let sub = match ctx.caller.leaf() {
            "spec" => NewSubcommand::Spec(NewSpecFlags {
                interview: ctx.flags.bool("interview"),
                non_interactive: ctx.flags.bool("non-interactive"),
                issue_source: crate::engine::issue::IssueSourceFlags {
                    issue: ctx.flags.string("issue"),
                },
            }),
            "workflow" => NewSubcommand::Workflow(NewWorkflowFlags {
                interview: ctx.flags.bool("interview"),
                non_interactive: ctx.flags.bool("non-interactive"),
                global: ctx.flags.bool("global"),
                format: ctx.flags.require_str("format")?,
            }),
            "skill" => {
                let pull = ctx.flags.string("pull");
                let pull_all = ctx.flags.bool("pull-all");
                let subdir = ctx.flags.string("subdir");
                // `--subdir` names a path *inside* a pulled repository, so it
                // is meaningless without one. The catalogue cannot say
                // "requires one of two flags", so the check lives here.
                if subdir.is_some() && pull.is_none() && !pull_all {
                    return Err(CommandError::InvalidFlagValue {
                        command: ctx.path().iter().map(|part| (*part).to_string()).collect(),
                        flag: "subdir".to_string(),
                        reason: "--subdir requires --pull <repo>".to_string(),
                    });
                }
                NewSubcommand::Skill(NewSkillFlags {
                    interview: ctx.flags.bool("interview"),
                    non_interactive: ctx.flags.bool("non-interactive"),
                    global: ctx.flags.bool("global"),
                    pull,
                    pull_all,
                    subdir,
                })
            }
            _ => return Err(CommandError::unknown_command(&ctx.path())),
        };
        Ok(Self::new(sub, ctx.engines.clone(), ctx.session.clone()))
    }

    pub fn subcommand(&self) -> &NewSubcommand {
        &self.sub
    }
}

#[async_trait]
impl Command for NewCommand {
    type Frontend = Box<dyn NewCommandFrontend>;
    type Outcome = NewOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        // One clone of the subcommand's flags so `self` can be lent to each
        // runner alongside them (WI 0114 F-51). The flag structs are three
        // `Option<String>`s and a bool apiece.
        let outcome = match self.sub.clone() {
            NewSubcommand::Spec(f) => spec::run(&self, f, frontend.as_mut()).await?,
            NewSubcommand::Workflow(f) => workflow::run(&self, f, frontend.as_mut()).await?,
            NewSubcommand::Skill(f) => skill::run(&self, f, frontend.as_mut()).await?,
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// Container-side path of the skill file the interview agent is told to edit.
///
/// Pairs with [`skill_interview_overlay`]: the skill's directory is mounted at
/// [`SKILL_INTERVIEW_CONTAINER_DIR`], so the file the host wrote at
/// `<dir>/SKILL.md` is reachable at `/awman/skill/SKILL.md` inside the
/// container. Naming the host path in the prompt instead would send the agent
/// to a path that does not exist there.
fn skill_interview_container_file(host_path: &std::path::Path) -> String {
    let name = host_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("SKILL.md");
    format!("{SKILL_INTERVIEW_CONTAINER_DIR}/{name}")
}

/// Structural mount for `new skill --interview`: the new skill's own
/// directory, read-write, at [`SKILL_INTERVIEW_CONTAINER_DIR`].
///
/// Not a user-supplied overlay — it is how the interview agent reaches the
/// only file it is asked to write, so its container path is fixed and the
/// prompt names it outright.
fn skill_interview_overlay(dir: &std::path::Path) -> crate::engine::overlay::DirectorySpec {
    crate::engine::overlay::DirectorySpec {
        host: dir.to_string_lossy().into_owned(),
        container: SKILL_INTERVIEW_CONTAINER_DIR.to_string(),
        permission: crate::engine::container::options::OverlayPermission::ReadWrite,
    }
}

fn pull_success_message(outcome: &PullOutcome) -> UserMessage {
    UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "Pulled '{}' into {} ({} skill(s) found under {}/): {}",
            outcome.slug,
            outcome.dir.display(),
            outcome.skills_found.len(),
            outcome.subdir,
            outcome.skills_found.join(", ")
        ),
    }
}

fn pull_library_outcome(outcome: PullOutcome) -> PullLibraryOutcome {
    PullLibraryOutcome {
        slug: outcome.slug,
        dir: outcome.dir.display().to_string(),
        updated: outcome.was_update,
        skills_found: outcome.skills_found,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeNewFrontend {
        workflow_name: String,
        workflow_title: String,
        steps: Vec<WorkflowStepInput>,
        step_index: std::sync::atomic::AtomicUsize,
        skill_name: String,
        skill_body: String,
    }
    impl FakeNewFrontend {
        fn new(workflow: &str, skill: &str, body: &str) -> Self {
            Self {
                workflow_name: workflow.into(),
                workflow_title: workflow.into(),
                steps: vec![WorkflowStepInput {
                    name: "step-1".into(),
                    agent: None,
                    model: None,
                    prompt: "do something".into(),
                }],
                step_index: std::sync::atomic::AtomicUsize::new(0),
                skill_name: skill.into(),
                skill_body: body.into(),
            }
        }
        fn with_steps(mut self, title: &str, steps: Vec<WorkflowStepInput>) -> Self {
            self.workflow_title = title.into();
            self.steps = steps;
            self
        }
    }
    impl crate::data::message::UserMessageSink for FakeNewFrontend {
        fn write_message(&mut self, _: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl crate::command::commands::mount_scope::MountScopeFrontend for FakeNewFrontend {
        fn ask_mount_scope(
            &mut self,
            _git_root: &std::path::Path,
            _cwd: &std::path::Path,
        ) -> Result<
            crate::command::commands::mount_scope::MountScopeDecision,
            crate::command::error::CommandError,
        > {
            Ok(crate::command::commands::mount_scope::MountScopeDecision::MountGitRoot)
        }
    }
    impl crate::command::commands::agent_setup::AgentSetupFrontend for FakeNewFrontend {
        fn ask_agent_setup(
            &mut self,
            _requested: &crate::data::session::AgentName,
            _default: &crate::data::session::AgentName,
            _default_available: bool,
            _image_only: bool,
        ) -> Result<
            crate::command::commands::agent_setup::AgentSetupDecision,
            crate::command::error::CommandError,
        > {
            Ok(crate::command::commands::agent_setup::AgentSetupDecision::Setup)
        }
        fn record_fallback(
            &mut self,
            _requested: &crate::data::session::AgentName,
            _fallback: &crate::data::session::AgentName,
        ) {
        }
    }
    impl crate::command::commands::agent_auth::AgentAuthFrontend for FakeNewFrontend {
        fn ask_agent_auth_consent(
            &mut self,
            _agent: &crate::data::session::AgentName,
            _env_var_names: &[&str],
        ) -> Result<
            crate::command::commands::agent_auth::AgentAuthDecision,
            crate::command::error::CommandError,
        > {
            Ok(crate::command::commands::agent_auth::AgentAuthDecision::DeclineOnce)
        }
    }
    /// The fake never launches a container; both methods are inert.
    impl crate::command::commands::agent_setup::HasAgentFrontend for FakeNewFrontend {
        fn container_frontend(
            &mut self,
        ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
            Box::new(crate::command::commands::agent_setup::NullAgentFrontend)
        }
    }

    impl crate::command::commands::agent_setup::AgentLaunchFrontend for FakeNewFrontend {
        fn set_pty_active(&mut self, _active: bool) {}
    }

    impl crate::command::commands::specs::SpecsCommandFrontend for FakeNewFrontend {}
    impl NewCommandFrontend for FakeNewFrontend {
        fn ask_workflow_name(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.workflow_name.clone())
        }
        fn ask_workflow_title(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.workflow_title.clone())
        }
        fn ask_workflow_step_name(
            &mut self,
        ) -> Result<String, crate::command::error::CommandError> {
            let idx = self.step_index.load(std::sync::atomic::Ordering::Relaxed);
            if idx < self.steps.len() {
                Ok(self.steps[idx].name.clone())
            } else {
                Ok(String::new())
            }
        }
        fn ask_workflow_step_agent(
            &mut self,
        ) -> Result<Option<String>, crate::command::error::CommandError> {
            let idx = self.step_index.load(std::sync::atomic::Ordering::Relaxed);
            if idx < self.steps.len() {
                Ok(self.steps[idx].agent.clone())
            } else {
                Ok(None)
            }
        }
        fn ask_workflow_step_model(
            &mut self,
        ) -> Result<Option<String>, crate::command::error::CommandError> {
            let idx = self.step_index.load(std::sync::atomic::Ordering::Relaxed);
            if idx < self.steps.len() {
                Ok(self.steps[idx].model.clone())
            } else {
                Ok(None)
            }
        }
        fn ask_workflow_step_prompt(
            &mut self,
        ) -> Result<String, crate::command::error::CommandError> {
            let idx = self.step_index.load(std::sync::atomic::Ordering::Relaxed);
            if idx < self.steps.len() {
                Ok(self.steps[idx].prompt.clone())
            } else {
                Ok(String::new())
            }
        }
        fn ask_add_another_step(&mut self) -> Result<bool, crate::command::error::CommandError> {
            let idx = self
                .step_index
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            Ok(idx < self.steps.len())
        }
        fn ask_skill_name(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.skill_name.clone())
        }
        fn ask_skill_body(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.skill_body.clone())
        }
    }

    #[tokio::test]
    async fn new_workflow_toml_writes_file_in_aspec_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = Engines::for_tests(tmp.path());
        let session = Session::for_tests(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Workflow(NewWorkflowFlags {
                interview: false,
                non_interactive: false,
                global: false,
                format: "toml".into(),
            }),
            engines,
            session,
        );
        let fe = FakeNewFrontend::new("my-wf", "skill", "").with_steps(
            "My Workflow",
            vec![
                WorkflowStepInput {
                    name: "plan".into(),
                    agent: None,
                    model: None,
                    prompt: "Plan the work.".into(),
                },
                WorkflowStepInput {
                    name: "implement".into(),
                    agent: Some("codex".into()),
                    model: Some("claude-opus-4-7".into()),
                    prompt: "Do the work.".into(),
                },
            ],
        );
        let outcome = cmd.run_with_frontend(Box::new(fe)).await.unwrap();
        if let NewOutcome::Workflow(w) = outcome {
            let path_str = w.path.expect("path must be Some");
            let path = std::path::Path::new(&path_str);
            assert!(
                path_str.contains("aspec/workflows/"),
                "path must be under aspec/workflows/: {path_str}"
            );
            assert!(path.exists(), "workflow file must exist: {path_str}");
            let content = std::fs::read_to_string(path).unwrap();
            assert!(
                content.contains("[[step]]"),
                "TOML workflow must contain [[step]]: {content}"
            );
            assert!(content.contains("plan"), "must contain step name 'plan'");
            assert!(
                content.contains("codex"),
                "must contain agent 'codex': {content}"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_workflow_yaml_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = Engines::for_tests(tmp.path());
        let session = Session::for_tests(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Workflow(NewWorkflowFlags {
                interview: false,
                non_interactive: false,
                global: false,
                format: "yaml".into(),
            }),
            engines,
            session,
        );
        let outcome = cmd
            .run_with_frontend(Box::new(FakeNewFrontend::new("my-wf", "skill", "")))
            .await
            .unwrap();
        if let NewOutcome::Workflow(w) = outcome {
            let path_str = w.path.expect("path must be Some");
            assert!(
                path_str.ends_with(".yaml"),
                "path must have .yaml extension: {path_str}"
            );
            assert!(
                path_str.contains("aspec/workflows/"),
                "path must be under aspec/workflows/: {path_str}"
            );
            let content = std::fs::read_to_string(&path_str).unwrap();
            assert!(
                content.contains("steps:"),
                "YAML workflow must contain steps key: {content}"
            );
            assert!(
                content.contains("step-1"),
                "must contain default step name: {content}"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_workflow_toml_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = Engines::for_tests(tmp.path());
        let session = Session::for_tests(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Workflow(NewWorkflowFlags {
                interview: false,
                non_interactive: false,
                global: false,
                format: "toml".into(),
            }),
            engines,
            session,
        );
        let outcome = cmd
            .run_with_frontend(Box::new(FakeNewFrontend::new("my-wf", "skill", "")))
            .await
            .unwrap();
        if let NewOutcome::Workflow(w) = outcome {
            let path_str = w.path.expect("path must be Some");
            assert!(
                path_str.ends_with(".toml"),
                "path must have .toml extension: {path_str}"
            );
            assert!(
                path_str.contains("aspec/workflows/"),
                "path must be under aspec/workflows/: {path_str}"
            );
            let content = std::fs::read_to_string(&path_str).unwrap();
            assert!(
                content.contains("[[step]]"),
                "TOML workflow must contain [[step]]: {content}"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_skill_writes_skill_md_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = Engines::for_tests(tmp.path());
        let session = Session::for_tests(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Skill(NewSkillFlags {
                interview: false,
                non_interactive: false,
                global: false,
                pull: None,
                pull_all: false,
                subdir: None,
            }),
            engines,
            session,
        );
        let outcome = cmd
            .run_with_frontend(Box::new(FakeNewFrontend::new(
                "wf",
                "my-skill",
                "Do something useful.",
            )))
            .await
            .unwrap();
        if let NewOutcome::Skill(s) = outcome {
            let path_str = s.path.expect("path must be Some");
            let path = std::path::Path::new(&path_str);
            assert!(path.exists(), "SKILL.md must exist: {path_str}");
            assert!(
                path.file_name().unwrap() == "SKILL.md",
                "file must be named SKILL.md"
            );
            let content = std::fs::read_to_string(path).unwrap();
            assert!(
                content.contains("my-skill"),
                "skill name must appear in SKILL.md"
            );
            assert!(
                content.contains("Do something useful."),
                "body must appear in SKILL.md"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_skill_empty_body_writes_default_skeleton() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = Engines::for_tests(tmp.path());
        let session = Session::for_tests(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Skill(NewSkillFlags {
                interview: false,
                non_interactive: false,
                global: false,
                pull: None,
                pull_all: false,
                subdir: None,
            }),
            engines,
            session,
        );
        let outcome = cmd
            .run_with_frontend(Box::new(FakeNewFrontend::new("wf", "my-skill", "")))
            .await
            .unwrap();
        if let NewOutcome::Skill(s) = outcome {
            let path_str = s.path.expect("path must be Some");
            let content = std::fs::read_to_string(&path_str).unwrap();
            assert!(
                content.contains("## Body"),
                "empty-body skill must contain ## Body skeleton: {content}"
            );
        } else {
            panic!("unexpected outcome variant");
        }
    }

    /// `new skill --interview` hands the skill file to an agent running in a
    /// container that only ever has the repo mounted at `/workspace`. A
    /// `--global` skill lives under `~/.awman/skills/`, which is nowhere
    /// inside that mount, so the skill's own directory has to be mounted at
    /// the fixed container path and the prompt has to name the file there —
    /// naming the host path sent the agent to a path the container has not
    /// got.
    #[test]
    fn the_skill_interview_mounts_the_skill_dir_and_names_it_in_the_prompt() {
        use crate::engine::container::options::OverlayPermission;

        let tmp = tempfile::tempdir().unwrap();
        // A global skill dir: deliberately outside any repo/workspace root.
        let dir = tmp.path().join("awman-home/skills/my-skill");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("SKILL.md");
        std::fs::write(&file, "# Skill: my-skill\n").unwrap();

        let overlay = skill_interview_overlay(&dir);
        assert_eq!(overlay.host, dir.to_string_lossy());
        assert_eq!(overlay.container, SKILL_INTERVIEW_CONTAINER_DIR);
        assert_eq!(
            overlay.permission,
            OverlayPermission::ReadWrite,
            "the interview agent has to write the skill file"
        );

        // The spec must survive the same resolution every overlay goes
        // through, and land at exactly the path the prompt names.
        let engines = Engines::for_tests(tmp.path());
        let resolved = engines
            .overlay_engine
            .resolve_user_overlay(&overlay, tmp.path(), None)
            .expect("the skill dir exists, so its overlay must resolve");
        assert_eq!(
            resolved.container_path,
            std::path::Path::new(SKILL_INTERVIEW_CONTAINER_DIR)
        );

        let prompt =
            render_skill_interview_prompt(&skill_interview_container_file(&file), "a summary");
        assert!(
            prompt.contains("/awman/skill/SKILL.md"),
            "the prompt must point at the mounted skill file: {prompt}"
        );
        assert!(
            !prompt.contains(&*dir.to_string_lossy()),
            "the host path must never reach the agent: {prompt}"
        );
    }
}

mod skill;
mod spec;
mod workflow;
