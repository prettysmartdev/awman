//! `NewCommand` — `new spec`, `new workflow`, `new skill`.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::chat::{open_session_for_cwd, resolve_agent};
use crate::command::commands::implement_prompts::{
    render_skill_interview_prompt, render_workflow_interview_prompt,
};
use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::fs::{SkillDirs, WorkflowDirs};
use crate::engine::agent::AgentRunOptions;
use crate::engine::container::options::ContainerOption;
use crate::engine::message::UserMessageSink;

#[derive(Debug, Clone)]
pub struct NewSpecFlags {
    pub interview: bool,
    pub non_interactive: bool,
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

#[derive(Debug, Clone, Serialize)]
pub struct NewSkillOutcome {
    pub interview: bool,
    pub global: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum NewOutcome {
    Spec(NewSpecOutcome),
    Workflow(NewWorkflowOutcome),
    Skill(NewSkillOutcome),
}

/// `NewCommandFrontend` extends `SpecsCommandFrontend` so the `Spec`
/// subcommand can drive the same Q&A as `specs new` (kind / title /
/// summary). Dispatch canonicalizes `specs new` to `new spec`, so this
/// branch *is* the implementation for both invocations.
pub trait NewCommandFrontend:
    UserMessageSink
    + crate::command::commands::specs::SpecsCommandFrontend
    + Send
    + Sync
{
    /// Prompt for a workflow name. CLI implementations gate on stdin TTY.
    fn ask_workflow_name(&mut self) -> Result<String, CommandError> {
        Ok("workflow".to_string())
    }
    /// Prompt for a one-line summary for the new workflow (used in interview mode).
    fn ask_workflow_summary(&mut self) -> Result<String, CommandError> {
        Ok(String::new())
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

pub struct NewCommand {
    sub: NewSubcommand,
    engines: Engines,
}

impl NewCommand {
    pub fn new(sub: NewSubcommand, engines: Engines) -> Self {
        Self { sub, engines }
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
        let outcome = match self.sub {
            NewSubcommand::Spec(f) => {
                // Delegate to the shared `create_new_spec` helper. Dispatch
                // canonicalizes `specs new` to `new spec`, so this branch is
                // the implementation for both invocations — Q&A, template
                // substitution, and the optional --interview agent run all
                // happen here.
                let new_outcome = crate::command::commands::specs::create_new_spec(
                    &self.engines,
                    f.interview,
                    f.non_interactive,
                    frontend.as_mut(),
                )
                .await?;
                NewOutcome::Spec(NewSpecOutcome {
                    interview: new_outcome.interview,
                    path: new_outcome.created_path,
                })
            }
            NewSubcommand::Workflow(f) => {
                let name = frontend
                    .ask_workflow_name()
                    .unwrap_or_else(|_| "workflow".into());
                let extension = match f.format.as_str() {
                    "yaml" => "yaml",
                    "yml" => "yml",
                    "md" | "markdown" => "md",
                    _ => "toml",
                };
                let session = if !f.global || f.interview {
                    Some(open_session_for_cwd(&self.engines)?)
                } else {
                    None
                };
                let git_root = session.as_ref().map(|s| s.git_root().to_path_buf());
                let workflow_dirs = WorkflowDirs::from_process_env(git_root)?;
                let dir = if f.global {
                    workflow_dirs.global_dir()
                } else {
                    workflow_dirs.repo_dir().unwrap()
                };
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("{name}.{extension}"));
                let body = match extension {
                    "yaml" | "yml" => format!("name: {name}\nsteps: []\n"),
                    "md" => format!("# Workflow: {name}\n\n## Steps\n"),
                    _ => "[[steps]]\nname = \"step-1\"\nagent = \"claude\"\nprompt = \"do something\"\n".to_string(),
                };
                let _ = std::fs::write(&path, body);

                if f.interview {
                    let session = session.as_ref().unwrap();
                    let agent = resolve_agent(&None, session)?;
                    let credentials = self
                        .engines
                        .auth_engine
                        .resolve_agent_auth(session, &agent)
                        .map_err(CommandError::from)?;
                    let summary = frontend.ask_workflow_summary().unwrap_or_default();
                    let filename = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&name)
                        .to_string();
                    let path_str = path.display().to_string();
                    let prompt = render_workflow_interview_prompt(&filename, &path_str, &summary);
                    let run_opts = AgentRunOptions {
                        initial_prompt: Some(prompt),
                        non_interactive: f.non_interactive,
                        env_passthrough: Some(session.effective_config().env_passthrough()),
                        ..Default::default()
                    };
                    let mut options = self
                        .engines
                        .agent_engine
                        .build_options(session, &agent, &run_opts)?;
                    if !credentials.env_vars.is_empty() {
                        options.push(ContainerOption::AgentCredentials {
                            env_vars: credentials.env_vars,
                        });
                    }
                    let instance = self.engines.runtime.build(options)?;
                    frontend.set_pty_active(true);
                    let cf = frontend.container_frontend();
                    let mut execution = match instance.run_with_frontend(cf) {
                        Ok(e) => e,
                        Err(e) => {
                            frontend.set_pty_active(false);
                            frontend.replay_queued();
                            return Err(CommandError::from(e));
                        }
                    };
                    let _ = execution.wait().await;
                    frontend.set_pty_active(false);
                    frontend.replay_queued();
                }

                NewOutcome::Workflow(NewWorkflowOutcome {
                    interview: f.interview,
                    global: f.global,
                    format: f.format,
                    path: Some(path.display().to_string()),
                })
            }
            NewSubcommand::Skill(f) => {
                let name = frontend.ask_skill_name().unwrap_or_else(|_| "skill".into());
                let session = if !f.global || f.interview {
                    Some(open_session_for_cwd(&self.engines)?)
                } else {
                    None
                };
                let git_root = session.as_ref().map(|s| s.git_root().to_path_buf());
                let skill_dirs = SkillDirs::from_process_env(git_root)?;
                let dir = if f.global {
                    skill_dirs.global_dir().join(&name)
                } else {
                    skill_dirs.repo_dir().unwrap().join(&name)
                };
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join("SKILL.md");

                if f.interview {
                    // Interview mode: write skeleton and let agent fill it in.
                    let skeleton = format!(
                        "# Skill: {name}\n\n## Description\n\n## Body\n"
                    );
                    let _ = std::fs::write(&path, skeleton);
                    let session = session.as_ref().unwrap();
                    let agent = resolve_agent(&None, session)?;
                    let credentials = self
                        .engines
                        .auth_engine
                        .resolve_agent_auth(session, &agent)
                        .map_err(CommandError::from)?;
                    let summary = frontend.ask_skill_summary().unwrap_or_default();
                    let path_str = path.display().to_string();
                    let prompt = render_skill_interview_prompt(&path_str, &summary);
                    let run_opts = AgentRunOptions {
                        initial_prompt: Some(prompt),
                        non_interactive: f.non_interactive,
                        env_passthrough: Some(session.effective_config().env_passthrough()),
                        ..Default::default()
                    };
                    let mut options = self
                        .engines
                        .agent_engine
                        .build_options(session, &agent, &run_opts)?;
                    if !credentials.env_vars.is_empty() {
                        options.push(ContainerOption::AgentCredentials {
                            env_vars: credentials.env_vars,
                        });
                    }
                    let instance = self.engines.runtime.build(options)?;
                    frontend.set_pty_active(true);
                    let cf = frontend.container_frontend();
                    let mut execution = match instance.run_with_frontend(cf) {
                        Ok(e) => e,
                        Err(e) => {
                            frontend.set_pty_active(false);
                            frontend.replay_queued();
                            return Err(CommandError::from(e));
                        }
                    };
                    let _ = execution.wait().await;
                    frontend.set_pty_active(false);
                    frontend.replay_queued();
                } else {
                    let body = frontend.ask_skill_body().unwrap_or_default();
                    let content = if body.is_empty() {
                        format!("# Skill: {name}\n\n## Description\n\n## Body\n")
                    } else {
                        format!("# Skill: {name}\n\n{body}\n")
                    };
                    let _ = std::fs::write(&path, content);
                }

                NewOutcome::Skill(NewSkillOutcome {
                    interview: f.interview,
                    global: f.global,
                    path: Some(path.display().to_string()),
                })
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

fn next_work_item_number(dir: &std::path::Path) -> u32 {
    let mut max = 0u32;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.len() >= 5 && s.as_bytes()[4] == b'-' {
                if let Ok(n) = s[..4].parse::<u32>() {
                    if n > max {
                        max = n;
                    }
                }
            }
        }
    }
    max + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_work_item_number_empty_dir_is_one() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(next_work_item_number(tmp.path()), 1);
    }

    #[test]
    fn next_work_item_number_finds_max_number() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("0001-first.md"), "").unwrap();
        std::fs::write(tmp.path().join("0010-tenth.md"), "").unwrap();
        std::fs::write(tmp.path().join("0005-fifth.md"), "").unwrap();
        assert_eq!(next_work_item_number(tmp.path()), 11);
    }

    struct FakeNewFrontend {
        workflow_name: String,
        skill_name: String,
        skill_body: String,
    }
    impl FakeNewFrontend {
        fn new(workflow: &str, skill: &str, body: &str) -> Self {
            Self {
                workflow_name: workflow.into(),
                skill_name: skill.into(),
                skill_body: body.into(),
            }
        }
    }
    impl crate::engine::message::UserMessageSink for FakeNewFrontend {
        fn write_message(&mut self, _: crate::engine::message::UserMessage) {}
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
        ) -> Result<crate::command::commands::agent_setup::AgentSetupDecision, crate::command::error::CommandError>
        {
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
        ) -> Result<crate::command::commands::agent_auth::AgentAuthDecision, crate::command::error::CommandError>
        {
            Ok(crate::command::commands::agent_auth::AgentAuthDecision::DeclineOnce)
        }
    }
    impl crate::command::commands::specs::SpecsCommandFrontend for FakeNewFrontend {}
    impl NewCommandFrontend for FakeNewFrontend {
        fn ask_workflow_name(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.workflow_name.clone())
        }
        fn ask_skill_name(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.skill_name.clone())
        }
        fn ask_skill_body(&mut self) -> Result<String, crate::command::error::CommandError> {
            Ok(self.skill_body.clone())
        }
    }

    fn make_engines(root: &std::path::Path) -> Engines {
        use std::sync::Arc;
        use crate::engine::overlay::OverlayEngine;
        use crate::engine::container::ContainerRuntime;
        use crate::data::fs::auth_paths::AuthPathResolver;
        use crate::data::fs::headless_paths::HeadlessPaths;
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            AuthPathResolver::at_home(root),
        ));
        let runtime = Arc::new(ContainerRuntime::docker());
        let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
            overlay.clone(),
            runtime.clone(),
        ));
        let auth_engine = Arc::new(crate::engine::auth::AuthEngine::with_paths(
            AuthPathResolver::at_home(root),
            HeadlessPaths::at_root(root),
        ));
        Engines {
            runtime,
            git_engine: Arc::new(crate::engine::git::GitEngine::new()),
            overlay_engine: overlay,
            auth_engine,
            agent_engine,
            workflow_state_store: Arc::new(crate::data::EngineWorkflowStateStore::at_git_root(root)),
        }
    }

    #[allow(clippy::await_holding_lock)]
    async fn with_cwd<F, Fut, T>(dir: &std::path::Path, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _lock = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
        std::env::set_current_dir(dir).unwrap();
        let result = f().await;
        let _ = std::env::set_current_dir(&prev);
        result
    }

    #[tokio::test]
    async fn new_workflow_toml_writes_file_in_aspec_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Workflow(NewWorkflowFlags {
                interview: false,
                non_interactive: false,
                global: false,
                format: "toml".into(),
            }),
            engines,
        );
        let outcome = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeNewFrontend::new("my-wf", "skill", "")))
                .await
                .unwrap()
        }).await;
        if let NewOutcome::Workflow(w) = outcome {
            let path_str = w.path.expect("path must be Some");
            let path = std::path::Path::new(&path_str);
            assert!(path.exists(), "workflow file must exist: {path_str}");
            let content = std::fs::read_to_string(path).unwrap();
            assert!(content.contains("[[steps]]"), "TOML workflow must contain [[steps]]");
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_workflow_yaml_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Workflow(NewWorkflowFlags {
                interview: false,
                non_interactive: false,
                global: false,
                format: "yaml".into(),
            }),
            engines,
        );
        let outcome = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeNewFrontend::new("my-wf", "skill", "")))
                .await
                .unwrap()
        }).await;
        if let NewOutcome::Workflow(w) = outcome {
            let path_str = w.path.expect("path must be Some");
            assert!(path_str.ends_with(".yaml"), "path must have .yaml extension: {path_str}");
            let content = std::fs::read_to_string(&path_str).unwrap();
            assert!(content.contains("steps:"), "YAML workflow must contain steps key");
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_workflow_md_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Workflow(NewWorkflowFlags {
                interview: false,
                non_interactive: false,
                global: false,
                format: "md".into(),
            }),
            engines,
        );
        let outcome = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeNewFrontend::new("my-wf", "skill", "")))
                .await
                .unwrap()
        }).await;
        if let NewOutcome::Workflow(w) = outcome {
            let path_str = w.path.expect("path must be Some");
            assert!(path_str.ends_with(".md"), "path must have .md extension: {path_str}");
            let content = std::fs::read_to_string(&path_str).unwrap();
            assert!(content.contains("## Steps"), "Markdown workflow must contain ## Steps");
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_skill_writes_skill_md_file() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Skill(NewSkillFlags {
                interview: false,
                non_interactive: false,
                global: false,
            }),
            engines,
        );
        let outcome = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeNewFrontend::new("wf", "my-skill", "Do something useful.")))
                .await
                .unwrap()
        }).await;
        if let NewOutcome::Skill(s) = outcome {
            let path_str = s.path.expect("path must be Some");
            let path = std::path::Path::new(&path_str);
            assert!(path.exists(), "SKILL.md must exist: {path_str}");
            assert!(
                path.file_name().unwrap() == "SKILL.md",
                "file must be named SKILL.md"
            );
            let content = std::fs::read_to_string(path).unwrap();
            assert!(content.contains("my-skill"), "skill name must appear in SKILL.md");
            assert!(content.contains("Do something useful."), "body must appear in SKILL.md");
        } else {
            panic!("unexpected outcome variant");
        }
    }

    #[tokio::test]
    async fn new_skill_empty_body_writes_default_skeleton() {
        let tmp = tempfile::tempdir().unwrap();
        let engines = make_engines(tmp.path());
        let cmd = NewCommand::new(
            NewSubcommand::Skill(NewSkillFlags {
                interview: false,
                non_interactive: false,
                global: false,
            }),
            engines,
        );
        let outcome = with_cwd(tmp.path(), || async {
            cmd.run_with_frontend(Box::new(FakeNewFrontend::new("wf", "my-skill", "")))
                .await
                .unwrap()
        }).await;
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
}
