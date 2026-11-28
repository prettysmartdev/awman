//! Squad task prerequisite scope (WI 0088).
//!
//! A task's captured `MountScope` — never an enclosing repository or the
//! caller's directory — decides which workflow prerequisites apply at every
//! checkpoint: the add/edit interview, the leader-prompt summary, the run
//! preflight and the generated-workflow narrowing. These tests drive the
//! production `SquadCommand` and `WorkflowPrerequisiteFlow` with private
//! sandbox state, a fake task gateway and a runtime that is never asked to
//! launch anything.
use crate::support;
use async_trait::async_trait;
use awman::command::commands::squad::commands::{
    SquadAddRequest, SquadCommand, SquadCommandFrontend, SquadOutcome, SquadSubcommand,
};
use awman::command::commands::squad::gateway::{CreateTask, DaemonStatus, TaskGateway, UpdateTask};
use awman::command::commands::workflow_prerequisites::{
    collect_task_prerequisites, IdentityChoice, PrerequisiteScope, TaskPrerequisitePlan,
    WorkflowPrerequisiteFlow, WorkflowPrerequisiteFlowOptions, WorkflowPrerequisiteFrontend,
};
use awman::command::commands::Command;
use awman::command::dispatch::{catalogue::FrontendKind, Engines};
use awman::command::error::CommandError;
use awman::data::config::env::EnvSnapshot;
use awman::data::config::workflows::{CredentialUse, PrerequisitePolicy};
use awman::data::fs::task_store::{MountScope, Run, Task, TaskStatus, TaskWorkspace};
use awman::data::fs::{ApiPaths, AuthPathResolver};
use awman::data::ids::TaskId;
use awman::data::message::{MessageLevel, UserMessage, UserMessageSink};
use awman::data::prompt::Prompt;
use awman::data::session::{AgentHandle, Session, SessionOpenOptions};
use awman::data::workflow_definition::{Workflow, WorkflowFormat};
use awman::data::workflow_prerequisites::{
    PrerequisiteOrigin, PrerequisiteStatus, WorkflowPrerequisite, WorkflowPrerequisites,
};
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::agent_runtime::{
    execution::AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ImageAcquisition,
    ResolvedAgentOptions,
};
use awman::engine::auth::AuthEngine;
use awman::engine::error::EngineError;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman_test_support::{fixtures::RepoBuilder, TestSandbox};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const PARENT_REMOTE: &str = "https://github.com/parent-owner/parent-repo.git";
const PARENT_IDENTITY: &str = "Parent Checkout Identity";

// ─── Fixtures ──────────────────────────────────────────────────────────────

/// A runtime the prerequisite checkpoints must never launch on.
struct InertRuntime;
impl AgentRuntimeEngine for InertRuntime {
    fn ready_agent(
        &self,
        _: &str,
        _: awman::engine::agent_runtime::ReadyAgentOptions,
        _: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        panic!("prerequisite checkpoints must not ready an agent")
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(None)
    }
    fn build_image(
        &self,
        _: &awman::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        panic!("prerequisite checkpoints must not build an image")
    }
    fn runtime_name(&self) -> &'static str {
        "docker"
    }
    fn display_name(&self) -> &'static str {
        "inert"
    }
    fn capabilities(&self) -> &Capabilities {
        static CAPS: Capabilities = Capabilities {
            arbitrary_host_mounts: true,
            kit_declarative: false,
            has_image_store: true,
            image_acquisition: ImageAcquisition::Build,
            per_launch_vm_settings: false,
            reports_resource_stats: false,
        };
        &CAPS
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("prerequisite checkpoints must not launch an agent")
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        panic!("prerequisite checkpoints must not read stats")
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        panic!("prerequisite checkpoints must not stop an agent")
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("prerequisite checkpoints must not attach")
    }
}

/// The sandbox env, plus a GitHub token a leaking checkpoint could pick up.
fn env_with_token(sandbox: &TestSandbox) -> EnvSnapshot {
    let mut vars: std::collections::BTreeMap<String, String> = sandbox
        .env()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    vars.insert("GH_TOKEN".into(), "ghp_caller_token_must_not_leak".into());
    EnvSnapshot::from_full_env(vars).with_invocation(sandbox.work(), vec!["awman".into()])
}

fn engines(sandbox: &TestSandbox, env: &EnvSnapshot) -> Engines {
    let runtime = Arc::new(InertRuntime);
    let auth = AuthPathResolver::at_home(sandbox.home());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()).with_env(env.clone()));
    Engines {
        runtime: runtime.clone(),
        container_runtime: None,
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::with_env(env.clone())),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(
            auth,
            ApiPaths::from_env(env).unwrap(),
        )),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(sandbox.work())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
        ephemeral_secrets: Arc::new(Default::default()),
        lease_workflow_review: None,
    }
}

/// A checkout with an `origin` remote and a repository-local identity — what
/// a directory task nested inside it must never see.
fn parent_checkout(sandbox: &TestSandbox) -> PathBuf {
    let repo = RepoBuilder::new(sandbox, "parent")
        .file("README.md", "parent")
        .file("nested/notes.md", "a plain directory inside the checkout")
        .init();
    repo.git(&["remote", "add", "origin", PARENT_REMOTE]);
    repo.git(&["config", "user.name", PARENT_IDENTITY]);
    repo.git(&["config", "user.email", "parent@checkout.invalid"]);
    std::fs::canonicalize(repo.path()).unwrap()
}

fn task(name: &str, root: &Path, mount_scope: MountScope) -> Task {
    let now = chrono::Utc::now();
    Task {
        id: TaskId::new(),
        name: name.into(),
        description: "d".into(),
        repo_scope: root.to_path_buf(),
        mount_scope,
        overlays: vec![],
        interval_secs: 3600,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
        trigger_requested_at: None,
        last_run_status: None,
        last_run_phase: None,
        unmet_env: vec![],
        prerequisites: None,
    }
}

fn session_for(plan: &TaskPrerequisitePlan, engines: &Engines, env: &EnvSnapshot) -> Session {
    plan.open_session(engines, Some(env.clone())).unwrap()
}

/// An unattended (squad daemon) flow under `plan`'s run scope.
fn run_flow(
    plan: &TaskPrerequisitePlan,
    engines: &Engines,
    session: &Session,
    policy: Option<&PrerequisitePolicy>,
) -> WorkflowPrerequisiteFlow {
    let mut options = WorkflowPrerequisiteFlowOptions::for_session(
        session,
        FrontendKind::SQUAD_LOCAL,
        &Default::default(),
        policy,
        PrerequisiteOrigin::AssumedAll,
        None,
    );
    options.scope = plan.run_scope();
    WorkflowPrerequisiteFlow::new(engines, session, options)
}

fn allowed_identity() -> PrerequisitePolicy {
    let mut policy = PrerequisitePolicy::default();
    policy.git_identity.source = awman::data::config::workflows::PolicySource::Task;
    policy.git_identity.use_ = CredentialUse::Allow;
    policy.git_identity.fixed = Some(awman::data::config::workflows::FixedIdentity {
        name: "Task Identity".into(),
        email: "task@identity.invalid".into(),
    });
    policy
}

const GIT_AND_PR_WORKFLOW: &str = r#"
[[steps]]
name = "work"
prompt = "work"
[[teardown]]
type = "commit_changes"
message = "m"
[[teardown]]
type = "push_branch"
[[teardown]]
type = "create_pull_request"
title = "t"
"#;

fn is_git_or_pr_only(p: &WorkflowPrerequisite) -> bool {
    use WorkflowPrerequisite::*;
    match p {
        GitRemote { .. }
        | PushAuth { .. }
        | PushAuthSsh { .. }
        | PushAuthHttps { .. }
        | GitHubToken { .. }
        | PushedBranch
        | NotDetachedHead => true,
        ContainerTool { name } => matches!(name.as_str(), "git" | "gh" | "ssh"),
        _ => false,
    }
}

/// A frontend with a person at it (or not), recording every question,
/// message and report.
#[derive(Default)]
struct Recorder {
    ask: bool,
    identity: Option<IdentityChoice>,
    identity_questions: usize,
    identity_prompt_text: Vec<String>,
    messages: Vec<UserMessage>,
    reports: Vec<awman::data::workflow_prerequisites::WorkflowPrerequisiteReport>,
}
impl Recorder {
    fn asking(identity: IdentityChoice) -> Self {
        Self {
            ask: true,
            identity: Some(identity),
            ..Default::default()
        }
    }
    fn warnings(&self) -> Vec<&str> {
        self.messages
            .iter()
            .filter(|m| matches!(m.level, MessageLevel::Warning | MessageLevel::Error))
            .map(|m| m.text.as_str())
            .collect()
    }
    fn all_text(&self) -> String {
        let mut text: Vec<String> = self.messages.iter().map(|m| m.text.clone()).collect();
        text.extend(self.identity_prompt_text.clone());
        for report in &self.reports {
            text.extend(report.render_lines());
        }
        text.join("\n")
    }
}
impl UserMessageSink for Recorder {
    fn write_message(&mut self, message: UserMessage) {
        self.messages.push(message);
    }
    fn replay_queued(&mut self) {}
}
impl WorkflowPrerequisiteFrontend for Recorder {
    fn can_ask_prerequisites(&self) -> bool {
        self.ask
    }
    fn show_workflow_prerequisites(
        &mut self,
        report: &awman::data::workflow_prerequisites::WorkflowPrerequisiteReport,
        _: awman::command::commands::workflow_prerequisites::PrerequisiteReportNext,
    ) -> Result<(), CommandError> {
        self.reports.push(report.clone());
        Ok(())
    }
    fn ask_prerequisite_identity(
        &mut self,
        prompt: &Prompt<IdentityChoice>,
    ) -> Result<IdentityChoice, CommandError> {
        self.identity_questions += 1;
        let mut text = vec![prompt.title.clone(), prompt.body.clone()];
        text.extend(prompt.choices.iter().map(|choice| choice.label.clone()));
        let text = text.join("\n");
        assert!(
            text.contains("Optional Git identity for future commits"),
            "{text}"
        );
        assert!(text.contains("Continue without an identity"), "{text}");
        assert!(
            !text.contains("will fail") && !text.contains("needs a git identity"),
            "{text}"
        );
        self.identity_prompt_text.push(text);
        // A directory task is never offered a write to repository config.
        assert!(
            !prompt
                .choices
                .iter()
                .any(|c| matches!(c.value, IdentityChoice::SetLocalFromHost { .. })),
            "a directory task must not be offered a repository-local identity write"
        );
        self.identity
            .clone()
            .ok_or_else(|| CommandError::Other("identity was not expected".into()))
    }
    // Every other credential question keeps the trait's error default, so
    // a directory task asked about SSH, tokens, remotes or tools fails here.
}
#[async_trait]
impl SquadCommandFrontend for Recorder {
    fn ask_edited_description(&mut self, current: &str) -> Result<String, CommandError> {
        Ok(current.to_string())
    }
    fn ask_edited_interval(&mut self, current: &str) -> Result<String, CommandError> {
        Ok(current.to_string())
    }
    fn ask_edited_agent(&mut self, current: Option<&str>) -> Result<Option<String>, CommandError> {
        Ok(current.map(str::to_string))
    }
    fn ask_edited_model(&mut self, current: Option<&str>) -> Result<Option<String>, CommandError> {
        Ok(current.map(str::to_string))
    }
    fn ask_replace_overlays(&mut self, _: &[String]) -> Result<bool, CommandError> {
        Ok(false)
    }
    fn ask_replace_agent_pool(
        &mut self,
        _: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<bool, CommandError> {
        Ok(false)
    }
}

/// A gateway recording what Layer 2 asks it to persist.
struct RecordingGateway {
    task: Mutex<Option<Task>>,
    created: Arc<Mutex<Vec<CreateTask>>>,
    updated: Arc<Mutex<Vec<UpdateTask>>>,
}
impl RecordingGateway {
    fn new(task: Option<Task>) -> Self {
        Self {
            task: Mutex::new(task),
            created: Default::default(),
            updated: Default::default(),
        }
    }
    fn stored(&self) -> Task {
        self.task.lock().unwrap().clone().expect("a stored task")
    }
}
#[async_trait]
impl TaskGateway for RecordingGateway {
    async fn create(&self, req: CreateTask) -> Result<Task, CommandError> {
        self.created.lock().unwrap().push(req.clone());
        let mut created = task(&req.name, Path::new("/created"), MountScope::Directory);
        created.prerequisites = req.prerequisites;
        Ok(created)
    }
    async fn update(&self, _: &str, req: UpdateTask) -> Result<Task, CommandError> {
        self.updated.lock().unwrap().push(req.clone());
        Ok(self.stored())
    }
    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        unimplemented!()
    }
    async fn get(&self, _: &str) -> Result<Task, CommandError> {
        Ok(self.stored())
    }
    async fn runs(&self, _: &str, _: usize) -> Result<Vec<Run>, CommandError> {
        unimplemented!()
    }
    async fn set_status(&self, _: &str, _: TaskStatus) -> Result<(), CommandError> {
        unimplemented!()
    }
    async fn trigger(&self, _: &str) -> Result<(), CommandError> {
        unimplemented!()
    }
    async fn cancel(&self, _: &str) -> Result<(), CommandError> {
        unimplemented!()
    }
    async fn delete(&self, _: &str) -> Result<(), CommandError> {
        unimplemented!()
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        unimplemented!()
    }
}

/// A recorder the command consumes, observed through a shared handle.
struct SharedRecorder(Arc<Mutex<Recorder>>);
impl UserMessageSink for SharedRecorder {
    fn write_message(&mut self, message: UserMessage) {
        self.0.lock().unwrap().write_message(message)
    }
    fn replay_queued(&mut self) {}
}
impl WorkflowPrerequisiteFrontend for SharedRecorder {
    fn can_ask_prerequisites(&self) -> bool {
        self.0.lock().unwrap().can_ask_prerequisites()
    }
    fn show_workflow_prerequisites(
        &mut self,
        report: &awman::data::workflow_prerequisites::WorkflowPrerequisiteReport,
        next: awman::command::commands::workflow_prerequisites::PrerequisiteReportNext,
    ) -> Result<(), CommandError> {
        self.0
            .lock()
            .unwrap()
            .show_workflow_prerequisites(report, next)
    }
    fn ask_prerequisite_identity(
        &mut self,
        prompt: &Prompt<IdentityChoice>,
    ) -> Result<IdentityChoice, CommandError> {
        self.0.lock().unwrap().ask_prerequisite_identity(prompt)
    }
}
#[async_trait]
impl SquadCommandFrontend for SharedRecorder {
    fn ask_edited_description(&mut self, current: &str) -> Result<String, CommandError> {
        self.0.lock().unwrap().ask_edited_description(current)
    }
    fn ask_edited_interval(&mut self, current: &str) -> Result<String, CommandError> {
        self.0.lock().unwrap().ask_edited_interval(current)
    }
    fn ask_edited_agent(&mut self, current: Option<&str>) -> Result<Option<String>, CommandError> {
        self.0.lock().unwrap().ask_edited_agent(current)
    }
    fn ask_edited_model(&mut self, current: Option<&str>) -> Result<Option<String>, CommandError> {
        self.0.lock().unwrap().ask_edited_model(current)
    }
    fn ask_replace_overlays(&mut self, current: &[String]) -> Result<bool, CommandError> {
        self.0.lock().unwrap().ask_replace_overlays(current)
    }
    fn ask_replace_agent_pool(
        &mut self,
        current: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<bool, CommandError> {
        self.0.lock().unwrap().ask_replace_agent_pool(current)
    }
}

fn add_request(name: &str, workspace: TaskWorkspace) -> SquadSubcommand {
    SquadSubcommand::Add(SquadAddRequest {
        interview: false,
        non_interactive: false,
        prefilled: Some(CreateTask {
            name: name.into(),
            description: "d".into(),
            workspace,
            mount_scope: MountScope::GitRoot,
            interval_secs: 3600,
            agent: None,
            model: None,
            overlays: vec![],
            agents_to_models: Default::default(),
            prerequisites: None,
        }),
    })
}

// ─── Interview (add / edit) ────────────────────────────────────────────────

/// `squad add` on the default workspace, run from inside a checkout: the
/// interview offers only the optional identity, is never rooted at the
/// caller's directory, and declining leaves no warning or instruction.
#[tokio::test]
async fn default_workspace_add_offers_only_the_optional_identity_from_any_cwd() {
    let sandbox = support::sandbox(
        "squad::prerequisites::default_workspace_add_offers_only_the_optional_identity_from_any_cwd",
    );
    let parent = parent_checkout(&sandbox);
    // The caller stands inside the checkout.
    let env = env_with_token(&sandbox).with_invocation(parent.clone(), vec!["awman".into()]);
    let engines = engines(&sandbox, &env);
    let gateway = RecordingGateway::new(None);
    let created = gateway.created.clone();
    let recorder = Arc::new(Mutex::new(Recorder::asking(IdentityChoice::Continue)));
    let outcome = SquadCommand::new(
        add_request("dir-task", TaskWorkspace::Default),
        Some(Box::new(gateway)),
        engines,
    )
    .with_env(env)
    .run_with_frontend(Box::new(SharedRecorder(recorder.clone())))
    .await
    .unwrap();
    assert!(matches!(outcome, SquadOutcome::Task(_)));

    let recorder = recorder.lock().unwrap();
    assert_eq!(
        recorder.identity_questions, 1,
        "the identity is offered once"
    );
    let report = recorder
        .reports
        .last()
        .expect("the interview shows its report");
    let asked: Vec<_> = report
        .checks
        .iter()
        .map(|c| c.prerequisite.clone())
        .collect();
    assert_eq!(asked, vec![WorkflowPrerequisite::GitIdentity]);
    assert!(
        recorder.warnings().is_empty(),
        "declining is not a gap: {:?}",
        recorder.warnings()
    );
    let text = recorder.all_text();
    for leaked in [
        PARENT_REMOTE,
        PARENT_IDENTITY,
        "ghp_caller_token_must_not_leak",
    ] {
        assert!(
            !text.contains(leaked),
            "{leaked} leaked into the interview:\n{text}"
        );
    }
    let policy = created.lock().unwrap()[0]
        .prerequisites
        .clone()
        .expect("the decisions are stored with the task");
    assert_ne!(policy.git_identity.use_, CredentialUse::Allow);
    assert_eq!(policy.ssh.use_, CredentialUse::default());
    assert_eq!(policy.github_token.use_, CredentialUse::default());
    sandbox.finish();
}

/// The identity is still an approved, optional choice: accepting the host
/// identity stores it as the task's fixed identity, and nothing else.
#[tokio::test]
async fn custom_directory_add_stores_a_chosen_identity_and_nothing_else() {
    let sandbox = support::sandbox(
        "squad::prerequisites::custom_directory_add_stores_a_chosen_identity_and_nothing_else",
    );
    let dir = sandbox.work().join("plain-folder");
    std::fs::create_dir_all(&dir).unwrap();
    let env = env_with_token(&sandbox);
    let engines = engines(&sandbox, &env);
    let gateway = RecordingGateway::new(None);
    let created = gateway.created.clone();
    let recorder = Arc::new(Mutex::new(Recorder::asking(IdentityChoice::UseHost {
        name: "Host Person".into(),
        email: "host@person.invalid".into(),
    })));
    SquadCommand::new(
        add_request("custom-dir", TaskWorkspace::Custom(dir)),
        Some(Box::new(gateway)),
        engines,
    )
    .with_env(env)
    .run_with_frontend(Box::new(SharedRecorder(recorder.clone())))
    .await
    .unwrap();
    let policy = created.lock().unwrap()[0].prerequisites.clone().unwrap();
    assert_eq!(policy.git_identity.use_, CredentialUse::Allow);
    assert_eq!(
        policy.git_identity.fixed.as_ref().map(|f| f.name.as_str()),
        Some("Host Person")
    );
    assert_eq!(policy.ssh.use_, CredentialUse::default());
    assert_eq!(policy.github_token.use_, CredentialUse::default());
    assert!(recorder.lock().unwrap().warnings().is_empty());
    sandbox.finish();
}

/// `squad edit --interview` of a directory task nested in a checkout keeps
/// its captured directory scope: the edit is not rebuilt as a repository
/// workspace, so the parent's remote and local identity never appear.
#[tokio::test]
async fn editing_a_nested_directory_task_keeps_its_captured_scope() {
    let sandbox = support::sandbox(
        "squad::prerequisites::editing_a_nested_directory_task_keeps_its_captured_scope",
    );
    let parent = parent_checkout(&sandbox);
    let nested = parent.join("nested");
    let env = env_with_token(&sandbox).with_invocation(parent.clone(), vec!["awman".into()]);
    let engines = engines(&sandbox, &env);
    let gateway = RecordingGateway::new(Some(task("nested", &nested, MountScope::Directory)));
    let updated = gateway.updated.clone();
    let recorder = Arc::new(Mutex::new(Recorder::asking(IdentityChoice::Continue)));
    SquadCommand::new(
        SquadSubcommand::Edit {
            name: "nested".into(),
            interview: true,
            update: UpdateTask::default(),
        },
        Some(Box::new(gateway)),
        engines,
    )
    .with_env(env)
    .run_with_frontend(Box::new(SharedRecorder(recorder.clone())))
    .await
    .unwrap();
    let recorder = recorder.lock().unwrap();
    let report = recorder.reports.last().unwrap();
    assert_eq!(report.checks.len(), 1);
    let identity = &report.checks[0];
    assert_eq!(identity.prerequisite, WorkflowPrerequisite::GitIdentity);
    // The parent's repository-local identity would have satisfied it.
    assert_ne!(identity.status, PrerequisiteStatus::Satisfied);
    let text = recorder.all_text();
    assert!(
        !text.contains(PARENT_REMOTE) && !text.contains(PARENT_IDENTITY),
        "{text}"
    );
    assert!(recorder.warnings().is_empty(), "{:?}", recorder.warnings());
    assert!(updated.lock().unwrap()[0].prerequisites.is_some());
    sandbox.finish();
}

/// A repository-backed task keeps every applicable assumed check: the
/// interview probes its remote and asks about credentials as before.
#[test]
fn repository_task_interview_keeps_every_assumed_check() {
    let sandbox = support::sandbox(
        "squad::prerequisites::repository_task_interview_keeps_every_assumed_check",
    );
    let repo = parent_checkout(&sandbox);
    let env = support::env_snapshot(&sandbox);
    let engines = engines(&sandbox, &env);
    let plan = TaskPrerequisitePlan::for_task(&task("repo", &repo, MountScope::GitRoot));
    assert_eq!(plan.interview_scope(), PrerequisiteScope::Repository);
    // Non-interactive: nobody is asked and nothing is stored.
    let mut nobody = Recorder::default();
    assert!(collect_task_prerequisites(
        &engines,
        FrontendKind::Cli,
        &plan,
        Some(env.clone()),
        &mut nobody
    )
    .unwrap()
    .is_none());
    let session = session_for(&plan, &engines, &env);
    let flow = run_flow(&plan, &engines, &session, None);
    let report = flow.preflight(&WorkflowPrerequisites::assume_all(), false);
    let remote = report
        .find(&WorkflowPrerequisite::GitRemote {
            remote: "origin".into(),
        })
        .expect("a repository task checks its remote");
    assert_eq!(remote.status, PrerequisiteStatus::Satisfied);
    assert!(remote
        .explanation
        .contains("github.com/parent-owner/parent-repo"));
    assert!(report
        .checks
        .iter()
        .any(|c| matches!(c.prerequisite, WorkflowPrerequisite::GitHubToken { .. })));
    // The repository's own identity is honoured for its own task.
    assert_eq!(
        report
            .find(&WorkflowPrerequisite::GitIdentity)
            .unwrap()
            .status,
        PrerequisiteStatus::Satisfied
    );
    sandbox.finish();
}

// ─── Leader summary ────────────────────────────────────────────────────────

/// A directory task's leader is never told a Git/PR step is unavailable
/// because it has no repository; an allowed identity shows only `commit`.
#[test]
fn directory_task_leader_summary_never_marks_git_steps_unavailable() {
    let sandbox = support::sandbox(
        "squad::prerequisites::directory_task_leader_summary_never_marks_git_steps_unavailable",
    );
    let parent = parent_checkout(&sandbox);
    let env = env_with_token(&sandbox);
    let engines = engines(&sandbox, &env);
    let default_workspace = sandbox.awman_home().join("squad/tasks/t/workspace");
    for root in [default_workspace, sandbox.work(), parent.join("nested")] {
        let plan = TaskPrerequisitePlan::captured(root.clone(), MountScope::Directory);
        let session = session_for(&plan, &engines, &env);
        // Unset identity: nothing applies, so the prompt gains no section.
        let unset = plan.leader_summary(&engines, &session, None);
        assert_eq!(unset, "", "{}: {unset}", root.display());
        // An allowed identity is the one thing worth telling the leader.
        let allowed = allowed_identity();
        let summary = plan.leader_summary(&engines, &session, Some(&allowed));
        assert_eq!(summary, "commit ✓", "{}", root.display());
    }
    sandbox.finish();
}

#[test]
fn repository_task_leader_summary_keeps_git_and_pr_steps() {
    let sandbox = support::sandbox(
        "squad::prerequisites::repository_task_leader_summary_keeps_git_and_pr_steps",
    );
    let repo = parent_checkout(&sandbox);
    let env = support::env_snapshot(&sandbox);
    let engines = engines(&sandbox, &env);
    let plan = TaskPrerequisitePlan::captured(repo, MountScope::GitRoot);
    let session = session_for(&plan, &engines, &env);
    let summary = plan.leader_summary(&engines, &session, None);
    for step in ["commit", "push", "create_pull_request"] {
        assert!(summary.contains(step), "{summary}");
    }
    sandbox.finish();
}

// ─── Run preflight and generated workflow ─────────────────────────────────

/// The run preflight and the generated-workflow narrowing of a directory
/// task omit every Git/PR-only check (not satisfied, not missing), grant no
/// credential and capture no remote — so a generated push step is left to
/// fail visibly as a step. Unset identity produces no warning.
#[test]
fn directory_task_run_omits_git_and_pr_checks_and_grants_nothing() {
    let sandbox = support::sandbox(
        "squad::prerequisites::directory_task_run_omits_git_and_pr_checks_and_grants_nothing",
    );
    let parent = parent_checkout(&sandbox);
    let env = env_with_token(&sandbox);
    let engines = engines(&sandbox, &env);
    let plan = TaskPrerequisitePlan::captured(parent.join("nested"), MountScope::Directory);
    let session = session_for(&plan, &engines, &env);
    let workflow = Workflow::parse(GIT_AND_PR_WORKFLOW, WorkflowFormat::Toml).unwrap();
    let needs = WorkflowPrerequisites::from_workflow(&workflow);
    assert!(needs
        .needs
        .iter()
        .any(|n| is_git_or_pr_only(&n.prerequisite)));

    let flow = run_flow(&plan, &engines, &session, None);
    let mut frontend = Recorder::default();
    let mut prepared = flow.prepare(&needs, &mut frontend).unwrap();
    assert!(
        prepared.report.checks.is_empty(),
        "{:?}",
        prepared.report.checks
    );
    assert!(prepared.remotes.is_empty(), "{:?}", prepared.remotes);
    assert!(prepared.grants.identity.is_none());
    assert!(prepared.grants.ssh.is_none());
    assert!(prepared.grants.token.is_none());
    assert!(prepared.flow.workflow_git_source().is_none());

    let (narrowed_needs, narrowed) = prepared
        .flow
        .narrow_after_generation(
            &workflow,
            &prepared.report,
            &mut prepared.grants,
            &prepared.redaction,
            &mut frontend,
        )
        .unwrap();
    assert!(narrowed_needs.needs.is_empty());
    assert!(narrowed.checks.is_empty());
    assert!(prepared.grants.token.is_none());
    assert!(frontend.warnings().is_empty(), "{:?}", frontend.warnings());
    let text = frontend.all_text();
    for leaked in [
        PARENT_REMOTE,
        PARENT_IDENTITY,
        "ghp_caller_token_must_not_leak",
    ] {
        assert!(
            !text.contains(leaked),
            "{leaked} leaked into the run:\n{text}"
        );
    }
    sandbox.finish();
}

/// With an approved identity, a directory task's commit step gets it — and
/// still no remote, push or token check or grant.
#[test]
fn directory_task_run_grants_only_an_approved_identity() {
    let sandbox = support::sandbox(
        "squad::prerequisites::directory_task_run_grants_only_an_approved_identity",
    );
    let env = env_with_token(&sandbox);
    let engines = engines(&sandbox, &env);
    let plan = TaskPrerequisitePlan::captured(sandbox.work(), MountScope::Directory);
    let session = session_for(&plan, &engines, &env);
    let workflow = Workflow::parse(GIT_AND_PR_WORKFLOW, WorkflowFormat::Toml).unwrap();
    let policy = allowed_identity();
    let flow = run_flow(&plan, &engines, &session, Some(&policy));
    let mut frontend = Recorder::default();
    let prepared = flow
        .prepare(
            &WorkflowPrerequisites::from_workflow(&workflow),
            &mut frontend,
        )
        .unwrap();
    let kinds: Vec<_> = prepared
        .report
        .checks
        .iter()
        .map(|c| c.prerequisite.clone())
        .collect();
    assert_eq!(kinds, vec![WorkflowPrerequisite::GitIdentity]);
    assert_eq!(
        prepared.report.checks[0].status,
        PrerequisiteStatus::Satisfied
    );
    assert_eq!(
        prepared.grants.identity.as_ref().map(|i| i.name.as_str()),
        Some("Task Identity")
    );
    assert!(prepared.grants.token.is_none() && prepared.grants.ssh.is_none());
    assert!(frontend.warnings().is_empty(), "{:?}", frontend.warnings());
    sandbox.finish();
}

/// A repository-backed task's run still checks its generated Git/PR steps.
#[test]
fn repository_task_run_keeps_generated_git_and_pr_checks() {
    let sandbox = support::sandbox(
        "squad::prerequisites::repository_task_run_keeps_generated_git_and_pr_checks",
    );
    let repo = parent_checkout(&sandbox);
    let env = support::env_snapshot(&sandbox);
    let engines = engines(&sandbox, &env);
    let plan = TaskPrerequisitePlan::captured(repo, MountScope::GitRoot);
    assert_eq!(plan.run_scope(), PrerequisiteScope::Repository);
    let session = session_for(&plan, &engines, &env);
    let workflow = Workflow::parse(GIT_AND_PR_WORKFLOW, WorkflowFormat::Toml).unwrap();
    let flow = run_flow(&plan, &engines, &session, None);
    let report = flow.preflight(&WorkflowPrerequisites::from_workflow(&workflow), false);
    assert!(report
        .checks
        .iter()
        .any(|c| is_git_or_pr_only(&c.prerequisite)));
    assert!(report
        .find(&WorkflowPrerequisite::GitRemote {
            remote: "origin".into()
        })
        .is_some());
    sandbox.finish();
}

/// Other callers keep the full assumed set: the flow's default scope is
/// `Repository` and `assume_all()` is unchanged.
#[test]
fn non_squad_flows_keep_the_full_assumed_set() {
    let sandbox =
        support::sandbox("squad::prerequisites::non_squad_flows_keep_the_full_assumed_set");
    let repo = parent_checkout(&sandbox);
    let env = support::env_snapshot(&sandbox);
    let session = Session::open_at_git_root(
        repo.clone(),
        repo,
        SessionOpenOptions {
            env: Some(env),
            ..Default::default()
        },
    )
    .unwrap();
    let options = WorkflowPrerequisiteFlowOptions::for_session(
        &session,
        FrontendKind::Cli,
        &Default::default(),
        None,
        PrerequisiteOrigin::AssumedAll,
        None,
    );
    assert_eq!(options.scope, PrerequisiteScope::Repository);
    let all = WorkflowPrerequisites::assume_all();
    assert_eq!(options.scope.project(&all, &options.policy), all);
    sandbox.finish();
}

/// An interview abandoned at the prerequisite step persists nothing.
#[tokio::test]
async fn abandoned_prerequisite_interview_persists_nothing() {
    let sandbox =
        support::sandbox("squad::prerequisites::abandoned_prerequisite_interview_persists_nothing");
    let env = support::env_snapshot(&sandbox);
    let engines = engines(&sandbox, &env);
    let gateway = RecordingGateway::new(None);
    let created = gateway.created.clone();
    // A person is there, but dismisses the identity question.
    let recorder = Arc::new(Mutex::new(Recorder {
        ask: true,
        ..Default::default()
    }));
    let result = SquadCommand::new(
        add_request("abandoned", TaskWorkspace::Default),
        Some(Box::new(gateway)),
        engines,
    )
    .with_env(env)
    .run_with_frontend(Box::new(SharedRecorder(recorder)))
    .await;
    assert!(result.is_err());
    assert!(
        created.lock().unwrap().is_empty(),
        "nothing reached the gateway"
    );
    assert!(!sandbox.awman_home().join("squad/tasks/abandoned").exists());
    sandbox.finish();
}

/// A repository-backed task whose repository is gone fails explicitly; it
/// is never quietly treated as a directory task.
#[test]
fn repository_task_whose_repository_is_gone_fails_explicitly() {
    let sandbox = support::sandbox(
        "squad::prerequisites::repository_task_whose_repository_is_gone_fails_explicitly",
    );
    let env = support::env_snapshot(&sandbox);
    let engines = engines(&sandbox, &env);
    let gone = sandbox.work().join("was-a-repo");
    std::fs::create_dir_all(&gone).unwrap();
    let plan = TaskPrerequisitePlan::captured(gone, MountScope::GitRoot);
    let error = plan
        .open_session(&engines, Some(env))
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot be resolved"), "{error}");
    sandbox.finish();
}

#[test]
fn removed_nested_repository_never_uses_its_parent() {
    let sandbox =
        support::sandbox("squad::prerequisites::removed_nested_repository_never_uses_its_parent");
    let parent = parent_checkout(&sandbox);
    let child = RepoBuilder::new(&sandbox, "parent/child")
        .file("child.txt", "child")
        .init();
    let captured = std::fs::canonicalize(child.path()).unwrap();
    let env = env_with_token(&sandbox);
    let engines = engines(&sandbox, &env);
    assert_eq!(
        std::fs::canonicalize(engines.git_engine.resolve_root(&captured).unwrap()).unwrap(),
        captured
    );
    std::fs::remove_dir_all(captured.join(".git")).unwrap();
    assert_eq!(
        std::fs::canonicalize(engines.git_engine.resolve_root(&captured).unwrap()).unwrap(),
        parent
    );
    for scope in [MountScope::GitRoot, MountScope::Cwd] {
        let error = TaskPrerequisitePlan::captured(captured.clone(), scope)
            .open_session(&engines, Some(env.clone()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("repository boundary changed"), "{error}");
        assert!(
            !error.contains(PARENT_REMOTE) && !error.contains(PARENT_IDENTITY),
            "{error}"
        );
    }
    sandbox.finish();
}

#[test]
fn interview_identity_never_becomes_a_run_wide_identity() {
    let sandbox = support::sandbox(
        "squad::prerequisites::interview_identity_never_becomes_a_run_wide_identity",
    );
    let env = env_with_token(&sandbox);
    let engines = engines(&sandbox, &env);
    let plan = TaskPrerequisitePlan::captured(sandbox.work(), MountScope::Directory);
    let mut frontend = Recorder::asking(IdentityChoice::UseHost {
        name: "Approved Identity".into(),
        email: "approved@example.invalid".into(),
    });
    let policy = collect_task_prerequisites(
        &engines,
        FrontendKind::Cli,
        &plan,
        Some(env.clone()),
        &mut frontend,
    )
    .unwrap()
    .unwrap();
    let session = session_for(&plan, &engines, &env);
    for workflow in [
        GIT_AND_PR_WORKFLOW,
        r#"[[steps]]
name = "work"
agent = "codex"
prompt = "work"
"#,
    ] {
        let workflow = Workflow::parse(workflow, WorkflowFormat::Toml).unwrap();
        let prepared = run_flow(&plan, &engines, &session, Some(&policy))
            .prepare(
                &WorkflowPrerequisites::from_workflow(&workflow),
                &mut Recorder::default(),
            )
            .unwrap();
        assert!(
            prepared.grants.run_identity.is_none(),
            "interview consent is only for commit containers"
        );
        assert!(prepared.grants.ssh.is_none() && prepared.grants.token.is_none());
    }
    sandbox.finish();
}

/// The evaluator's actual workflow frontend interface, with a structured
/// message stream we can inspect across the production command handoff.
struct WorkflowFrontends {
    bus: awman::command::commands::api_server::event_bus::EventBus,
}
impl awman::command::commands::squad::evaluation::SquadRunFrontends for WorkflowFrontends {
    fn leader_frontend(
        &self,
        _: &str,
        _: &awman::data::ids::RunId,
        _: &Path,
        _: &str,
        _: awman::command::commands::mount_scope::MountScopeDecision,
        _: awman::command::interaction::InteractionDecision,
    ) -> Result<Box<dyn awman::engine::agent_runtime::frontend::AgentFrontend>, CommandError> {
        panic!("this test starts at the validated generated-workflow handoff")
    }
    fn workflow_frontend(
        &self,
        _: &str,
        _: &awman::data::ids::RunId,
        _: &Path,
        _: awman::command::commands::mount_scope::MountScopeDecision,
        _: awman::command::interaction::InteractionDecision,
    ) -> Result<
        Box<dyn awman::command::commands::exec_workflow::ExecWorkflowCommandFrontend>,
        CommandError,
    > {
        Ok(Box::new(
            awman::frontend::api::command_frontend::ApiDispatchFrontend::new(
                "exec workflow",
                &[],
                self.bus.sender(),
            ),
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_directory_workflow_reaches_teardown_without_prerequisite_warnings_or_caller_grants(
) {
    use awman::command::commands::squad::evaluation::LocalTaskEvaluator;
    use awman::data::config::daemon::Interaction;
    use awman::engine::container::ContainerRuntime;
    use awman::engine::squad::evaluator::{EvaluationRequest, NoRunProgress};
    let mut sandbox = support::sandbox("squad::prerequisites::generated_directory_workflow_reaches_teardown_without_prerequisite_warnings_or_caller_grants");
    let private_root = sandbox.root().to_path_buf();
    for key in ["TMPDIR", "TEMP", "TMP"] {
        sandbox.env_mut().set(key, &private_root);
    }
    let docker = sandbox.install_fake(support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    docker
        .on(&["image", "inspect"])
        .stdout("/home/awman\n")
        .exit(0);
    docker.on(&["run"]).exit(0);
    docker
        .on(&["run", "-d"])
        .stdout("owned-phase-container\n")
        .exit(0);
    docker.on(&["exec"]).exit(0);
    let env = env_with_token(&sandbox);
    let parent = parent_checkout(&sandbox);
    let plain = sandbox.work().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let default = sandbox.awman_home().join("squad/tasks/default/workspace");
    std::fs::create_dir_all(&default).unwrap();
    let ssh_dir = sandbox.home().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::fs::write(ssh_dir.join("id_ed25519"), "test-secret-private-key-canary").unwrap();
    let explicit = sandbox.work().join("explicit-overlays");
    std::fs::create_dir_all(&explicit).unwrap();
    let mut automatic_calls = Vec::new();
    let runtime = Arc::new(ContainerRuntime::docker_in(env.clone()));
    let mut engines = engines(&sandbox, &env);
    engines.runtime = runtime.clone();
    engines.container_runtime = Some(runtime.clone());
    engines.agent_engine = Arc::new(AgentEngine::new(engines.overlay_engine.clone(), runtime));
    #[cfg(unix)]
    {
        use awman::data::{
            git_transport::{GitAuthKind, GitAuthSpec},
            secret::SecretString,
        };
        use awman::engine::git::{
            credential_socket::CacheSocketListener, lease_transport::CredentialScope,
        };
        let listener = Arc::new(
            CacheSocketListener::start(
                &sandbox.root().join("r"),
                Arc::new(
                    GitAuthSpec::new(
                        GitAuthKind::Token,
                        None,
                        SecretString::new("test-secret-caller-lease"),
                    )
                    .unwrap(),
                ),
                CredentialScope::for_http_url(PARENT_REMOTE).unwrap(),
            )
            .unwrap(),
        );
        engines.ephemeral_secrets = Arc::new(
            engines
                .ephemeral_secrets
                .with_container_git_socket(Some(listener)),
        );
    }
    for (index, root) in [default, plain, parent.join("nested"), explicit]
        .into_iter()
        .enumerate()
    {
        std::fs::create_dir_all(root.join(".awman")).unwrap();
        // The session uses the canonical workspace path. On Windows that has
        // an extended-length prefix, so fake CLI rules must use it too.
        let root = std::fs::canonicalize(root).unwrap();
        std::fs::write(root.join(".awman/Dockerfile.codex"), "FROM scratch\n").unwrap();
        let before_calls = docker.calls().len();
        let plan = TaskPrerequisitePlan::captured(root.clone(), MountScope::Directory);
        let session = session_for(&plan, &engines, &env);
        let run_dir = sandbox.work().join(format!("run-{index}"));
        std::fs::create_dir_all(&run_dir).unwrap();
        let workflow_path = run_dir.join("workflow.toml");
        std::fs::write(
            &workflow_path,
            r#"
title = "directory-handoff"
[[steps]]
name = "work"
agent = "codex"
prompt = "work"
[[teardown]]
type = "run_shell"
command = "echo unrelated-phase"
[[teardown]]
type = "commit_changes"
message = "commit in generated clone"
abort_on_failure = false
[[teardown]]
type = "create_pull_request"
title = "generated PR"
body = "generated"
abort_on_failure = false
[[teardown]]
type = "push_branch"
abort_on_failure = false
"#,
        )
        .unwrap();
        let generated = Workflow::parse(
            &std::fs::read_to_string(&workflow_path).unwrap(),
            WorkflowFormat::Toml,
        )
        .unwrap();
        if index < 3 {
            for step in generated.teardown.iter().skip(2) {
                let (script, env) =
                    awman::engine::workflow::step_commands::teardown_step_to_shell(&step.step);
                let mut args = vec!["exec".to_string(), "-w".into(), root.display().to_string()];
                for (name, value) in env.unwrap_or_default() {
                    args.extend(["-e".into(), format!("{name}={value}")]);
                }
                args.extend([
                    "owned-phase-container".into(),
                    "sh".into(),
                    "-c".into(),
                    script,
                ]);
                docker
                    .on(&args.iter().map(String::as_str).collect::<Vec<_>>())
                    .stderr("credentials missing for generated Git/PR step\n")
                    .exit(128);
            }
        }
        let bus = awman::command::commands::api_server::event_bus::EventBus::new(2048);
        let mut events = bus.subscribe();
        let evaluator = LocalTaskEvaluator::new(
            engines.clone(),
            env.clone(),
            Arc::new(WorkflowFrontends { bus }),
        );
        let mut request = EvaluationRequest {
            task: task(&format!("task-{index}"), &root, MountScope::Directory),
            run_id: awman::data::ids::RunId::new(),
            task_dir: root.clone(),
            run_log_dir: run_dir.clone(),
            guidance: None,
            agents_to_models: None,
            default_leader: None,
            progress: Arc::new(NoRunProgress),
        };
        if index == 3 {
            request.task.overlays = vec!["env(GH_TOKEN)".into(), "ssh()".into()];
        }
        let interaction = awman::command::interaction::InteractionDecision {
            interaction: Interaction::Headless,
            pty_size: None,
        };
        let outcome = evaluator
            .execute_generated_workflow(&request, session, &workflow_path, interaction)
            .await;
        let mut output = String::new();
        while let Ok(event) = events.try_recv() {
            output.push_str(&serde_json::to_string(&event).unwrap());
        }
        assert!(outcome.is_ok(), "{outcome:?}\n{output}");
        let outcome = outcome.unwrap();
        if index < 3 {
            assert_ne!(
                outcome.exit_code,
                Some(0),
                "actual Git/PR failures must fail the run"
            );
            let state = WorkflowStateStore::at_git_root(run_dir.clone())
                .load(None, "directory-handoff")
                .unwrap()
                .expect("persisted workflow state");
            assert_eq!(
                state.teardown_step_states.len(),
                4,
                "generated steps must remain present"
            );
            assert!(matches!(
                state.teardown_step_states[0].status,
                awman::data::workflow_state::PhaseStepStatus::Succeeded
            ));
            for step in &state.teardown_step_states[2..] {
                assert!(
                    matches!(&step.status, awman::data::workflow_state::PhaseStepStatus::Failed { error } if error.contains("credentials missing")),
                    "{step:?}"
                );
            }
        }
        assert!(!output.contains("prerequisite policy not set"), "{output}");
        assert!(
            !output.contains("workflow prerequisite not satisfied"),
            "{output}"
        );
        assert!(output.contains("teardown"), "must reach teardown: {output}");
        assert!(
            !output.contains(PARENT_REMOTE) && !output.contains(PARENT_IDENTITY),
            "{output}"
        );
        let calls: Vec<_> = docker.calls().into_iter().skip(before_calls).collect();
        if index == 3 {
            assert!(
                calls
                    .iter()
                    .any(|c| c.argv.iter().any(|a| a.contains("GH_TOKEN"))),
                "explicit token overlay must reach a launch"
            );
            assert!(
                calls.iter().any(|c| c
                    .argv
                    .iter()
                    .any(|a| a.contains(&ssh_dir.display().to_string()))),
                "explicit SSH overlay must reach a launch"
            );
        } else {
            automatic_calls.extend(calls);
        }
    }
    let calls = automatic_calls;
    let launches: Vec<_> = calls
        .iter()
        .filter(|call| matches!(call.argv.first().map(String::as_str), Some("run" | "exec")))
        .collect();
    assert!(
        !launches.is_empty(),
        "production runtime boundary must be exercised"
    );
    for call in launches {
        assert!(
            !call.argv.iter().any(|arg| arg.contains("awman_push_key")
                || arg.contains("git-credentials")
                || arg.contains("/awman/git-auth")
                || arg.contains("credential-cache")
                || arg.contains(&ssh_dir.display().to_string())),
            "{:?}",
            call.argv
        );
        assert!(
            !call
                .argv
                .iter()
                .any(|arg| arg.contains("GH_TOKEN") || arg.contains("GITHUB_TOKEN")),
            "automatic tokens must not reach a directory run"
        );
        assert!(
            !call
                .argv
                .iter()
                .any(|arg| arg.contains("GIT_AUTHOR_NAME") || arg.contains("GIT_COMMITTER_NAME")),
            "unset identity must stay unset"
        );
    }
    drop(engines);
    sandbox.finish();
}
