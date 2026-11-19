//! WI 0152: `exec prompt --read-only-workspace` and the per-step
//! `read_only_workspace` option of `exec workflow`, proven through the
//! production commands against a recording runtime.
//!
//! The recording runtime captures the resolved container options the command
//! layer hands to the runtime (the only thing a real runtime consumes) and
//! then fails the build, so nothing is ever launched. Real filesystem
//! enforcement is the boundary tier's job (`tests/boundary/*`).
use crate::support;
use awman::command::{
    commands::{
        api_server::event_bus::EventBus,
        chat::{ChatCommand, ChatCommandFlags},
        exec_prompt::{ExecPromptCommand, ExecPromptCommandFlags},
        exec_workflow::{ExecWorkflowCommand, ExecWorkflowCommandFlags},
        Command,
    },
    dispatch::{catalogue::CommandCatalogue, Engines},
    error::CommandError,
};
use awman::data::{
    config::flags::PrerequisiteFlags,
    fs::{ApiPaths, AuthPathResolver},
    session::{AgentHandle, Session, SessionOpenOptions},
    workspace::{WorkspaceAccess, READ_ONLY_WORKSPACE_FLAG},
    WorkflowStateStore,
};
use awman::engine::{
    agent::AgentEngine,
    agent_runtime::{
        execution::AgentInstance, AgentRuntimeEngine, AgentStats, Capabilities, ImageAcquisition,
        ResolvedAgentOptions,
    },
    auth::AuthEngine,
    container::options::OverlayPermission,
    error::EngineError,
    git::GitEngine,
    overlay::OverlayEngine,
};
use awman::frontend::api::command_frontend::ApiDispatchFrontend;
use awman_test_support::{
    fixtures::{ConfigTree, RepoBuilder},
    TestSandbox,
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// What one launch asked of the runtime.
#[derive(Debug, Clone)]
struct Launch {
    overlays: Vec<(String, OverlayPermission)>,
    tmpfs: Vec<PathBuf>,
    read_only_workspace: bool,
    /// Seeded prompt, inline system prompt and the content of every system
    /// prompt file, concatenated: whatever text the agent is told.
    prompt_text: String,
    tmpdir: Option<String>,
    /// Native mode flags (WI 0070): never added by the mount flag.
    mode_flags: Vec<String>,
}

impl Launch {
    fn permission(&self, guest: &str) -> Option<OverlayPermission> {
        self.overlays
            .iter()
            .find(|(path, _)| path == guest)
            .map(|(_, permission)| *permission)
    }
    fn workspace_is_read_only(&self) -> bool {
        self.permission("/workspace") == Some(OverlayPermission::ReadOnly)
    }
}

struct Runtime {
    caps: Capabilities,
    launches: Mutex<Vec<Launch>>,
    builds: Mutex<usize>,
}

fn capabilities(arbitrary_host_mounts: bool) -> Capabilities {
    Capabilities {
        arbitrary_host_mounts,
        kit_declarative: !arbitrary_host_mounts,
        has_image_store: arbitrary_host_mounts,
        image_acquisition: if arbitrary_host_mounts {
            ImageAcquisition::Build
        } else {
            ImageAcquisition::Kit
        },
        per_launch_vm_settings: false,
        reports_resource_stats: false,
    }
}

impl AgentRuntimeEngine for Runtime {
    fn ready_agent(
        &self,
        _: &str,
        _: awman::engine::agent_runtime::ReadyAgentOptions,
        _: &mut dyn awman::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(Some("/home/awman".into()))
    }
    fn build_image(
        &self,
        _: &awman::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        panic!("a read-only refusal must precede image build")
    }
    fn runtime_name(&self) -> &'static str {
        "test"
    }
    fn display_name(&self) -> &'static str {
        "recording runtime"
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        *self.builds.lock().unwrap() += 1;
        if let ResolvedAgentOptions::Container(options) = options {
            let mut prompt_text = options.seeded_prompt.clone().unwrap_or_default();
            if let Some((_, text)) = &options.system_prompt_inline {
                prompt_text.push('\n');
                prompt_text.push_str(text);
            }
            for host in options
                .system_prompt_file
                .iter()
                .map(|(host, _, _)| host)
                .chain(
                    options
                        .system_prompt_env_file
                        .iter()
                        .map(|(_, host, _)| host),
                )
            {
                prompt_text.push('\n');
                prompt_text.push_str(&std::fs::read_to_string(host).unwrap_or_default());
            }
            self.launches.lock().unwrap().push(Launch {
                overlays: options
                    .overlays
                    .iter()
                    .map(|o| (o.container_path.display().to_string(), o.permission))
                    .collect(),
                tmpfs: options.tmpfs.clone(),
                read_only_workspace: options.read_only_workspace,
                prompt_text,
                tmpdir: options
                    .env_literal
                    .iter()
                    .find(|e| e.key == "TMPDIR")
                    .map(|e| e.value.clone()),
                mode_flags: options.agent_mode_flags.clone(),
            });
        }
        Err(EngineError::Other("captured by test runtime".into()))
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("runtime must not be reached")
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("runtime must not be reached")
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("runtime must not be reached")
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        panic!("runtime must not be reached")
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        panic!("runtime must not be reached")
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("runtime must not be reached")
    }
}

struct Fixture {
    session: Session,
    engines: Engines,
    runtime: Arc<Runtime>,
}

impl Fixture {
    fn launches(&self) -> Vec<Launch> {
        self.runtime.launches.lock().unwrap().clone()
    }
    fn builds(&self) -> usize {
        *self.runtime.builds.lock().unwrap()
    }
}

/// A repository, session and engines over a runtime that can (`true`) or
/// cannot (`false`, the sbx shape) mount host paths read-only.
fn fixture(s: &TestSandbox, name: &str, workflow: &str, can_mount: bool) -> Fixture {
    let repo = RepoBuilder::new(s, name)
        .file("README.md", "fixture\n")
        .file("workflow.toml", workflow)
        .init();
    ConfigTree::new()
        .global(json!({"agents": {"auth":{"refresh":{"enabled":false}}}}))
        .write(s, Some(repo.path()));
    let session = Session::open_at_git_root(
        repo.path().to_path_buf(),
        repo.path().to_path_buf(),
        SessionOpenOptions {
            env: Some(support::env_snapshot(s)),
            ..Default::default()
        },
    )
    .unwrap();
    let env = session.env().clone();
    let runtime = Arc::new(Runtime {
        caps: capabilities(can_mount),
        launches: Default::default(),
        builds: Default::default(),
    });
    let auth = AuthPathResolver::at_home(s.home());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: None,
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::with_env(env.clone())),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(
            auth,
            ApiPaths::from_env(&env).unwrap(),
        )),
        agent_engine: Arc::new(
            AgentEngine::new(overlay, runtime.clone())
                .with_lease_workspace_scope(session.git_root().to_path_buf()),
        ),
        workflow_state_store: Arc::new(WorkflowStateStore::new(&session)),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
        ephemeral_secrets: Arc::new(Default::default()),
        lease_workflow_review: Some(Arc::new(
            awman::command::worker::policy::LeaseWorkflowReview::foreground(),
        )),
    };
    Fixture {
        session,
        engines,
        runtime,
    }
}

const ONE_STEP: &str = "title = 'one'\n\n[[step]]\nname = 'run'\nprompt = 'look around'\n";
const STEP_MARKED: &str =
    "title = 'one'\n\n[[step]]\nname = 'run'\nprompt = 'look around'\nread_only_workspace = true\n";
/// A writable first step and a read-only second step.
const LATE_RO_STEP: &str = "title = 'two'\n\n[[step]]\nname = 'build'\nprompt = 'build'\n\n[[step]]\nname = 'review'\nprompt = 'review'\ndepends_on = ['build']\nread_only_workspace = true\n";
const WITH_SETUP: &str = "title = 'setup'\n\n[[setup]]\ntype = 'run_shell'\ncommand = 'true'\n\n[[step]]\nname = 'run'\nprompt = 'look'\n";

fn prompt_flags(allow_docker: bool) -> ExecPromptCommandFlags {
    ExecPromptCommandFlags {
        prompt: Some("review the repository".into()),
        work_item: None,
        non_interactive: true,
        plan: false,
        allow_docker,
        yolo: false,
        yolo_authorized: false,
        auto: false,
        auto_explicit: false,
        agent: Some("claude".into()),
        model: None,
        launch_mode: None,
        overlay: vec![],
        agent_flags: None,
        session_worktree: None,
        automation: None,
    }
}

fn workflow_flags(allow_docker: bool, dynamic: bool) -> ExecWorkflowCommandFlags {
    ExecWorkflowCommandFlags {
        workflow: (!dynamic).then(|| "workflow.toml".into()),
        work_item: None,
        non_interactive: true,
        plan: false,
        allow_docker,
        worktree: false,
        yolo: false,
        yolo_authorized: false,
        auto: true,
        agent: Some("claude".into()),
        model: None,
        launch_mode: None,
        overlay: vec![],
        agent_flags: None,
        max_concurrent: None,
        dynamic,
        leader: None,
        on_dirty_merge_target: None,
        on_merge_failure: None,
        on_existing_worktree: None,
        prerequisites: Box::new(PrerequisiteFlags::default()),
        squad_task_prerequisites: None,
    }
}

/// Run `exec prompt` through the API frontend; returns the result and every
/// logged event, serialized.
async fn run_prompt(
    f: &Fixture,
    access: WorkspaceAccess,
    allow_docker: bool,
    output_dir: Option<PathBuf>,
) -> (Result<(), CommandError>, String) {
    let bus = EventBus::new(1024);
    let mut log = bus.subscribe_log();
    let frontend = ApiDispatchFrontend::new("exec prompt", &[], bus.sender());
    let result = ExecPromptCommand::new(
        prompt_flags(allow_docker),
        f.engines.clone(),
        f.session.clone(),
    )
    .with_workspace_access(access)
    .with_output_dir(output_dir)
    .run_with_frontend(Box::new(frontend))
    .await
    .map(|_| ());
    (result, drain(&mut log))
}

async fn run_workflow(
    f: &Fixture,
    access: WorkspaceAccess,
    allow_docker: bool,
    dynamic: bool,
) -> (Result<(), CommandError>, String) {
    let bus = EventBus::new(1024);
    let mut log = bus.subscribe_log();
    let frontend = ApiDispatchFrontend::new("exec workflow", &[], bus.sender());
    let result = ExecWorkflowCommand::new(
        workflow_flags(allow_docker, dynamic),
        f.engines.clone(),
        f.session.clone(),
    )
    .with_workspace_access(access)
    .run_with_frontend(Box::new(frontend))
    .await
    .map(|_| ());
    (result, drain(&mut log))
}

fn drain(log: &mut tokio::sync::mpsc::UnboundedReceiver<impl serde::Serialize>) -> String {
    let mut output = String::new();
    while let Ok(event) = log.try_recv() {
        output.push_str(&serde_json::to_string(&event).unwrap());
    }
    output
}

/// The one sentence every read-only launch tells the agent.
const PROMPT_LINE: &str = "The workspace at /workspace is mounted read-only";
const REFUSAL: &str = "cannot mount the workspace read-only";

#[tokio::test(flavor = "multi_thread")]
async fn exec_prompt_read_only_launch_mounts_ro_tmpfs_keeps_output_writable_and_tells_the_agent() {
    let s = support::sandbox("command::read_only_workspace::exec_prompt_read_only_launch_mounts_ro_tmpfs_keeps_output_writable_and_tells_the_agent");
    let f = fixture(&s, "prompt-ro", ONE_STEP, true);
    let output = s.root().join("api-output");
    let (_, events) = run_prompt(&f, WorkspaceAccess::ReadOnly, false, Some(output.clone())).await;

    let launches = f.launches();
    assert_eq!(launches.len(), 1, "{events}");
    let launch = &launches[0];
    assert!(launch.read_only_workspace);
    assert!(launch.workspace_is_read_only(), "{launch:?}");
    assert_eq!(launch.tmpfs, [PathBuf::from("/tmp")]);
    assert_eq!(launch.tmpdir.as_deref(), Some("/tmp"));
    // /awman/output stays writable; nothing overlapping /workspace is.
    assert_eq!(
        launch.permission("/awman/output"),
        Some(OverlayPermission::ReadWrite),
        "{launch:?}"
    );
    for (guest, permission) in &launch.overlays {
        if guest.starts_with("/workspace") {
            assert_eq!(*permission, OverlayPermission::ReadOnly, "{guest}");
        }
    }
    // The prompt line names the writable places, output first.
    assert!(launch.prompt_text.contains(PROMPT_LINE), "{launch:?}");
    assert!(launch.prompt_text.contains("/awman/output"));
    assert!(launch.prompt_text.contains("/tmp"));
    // The user's prompt is untouched.
    assert!(launch.prompt_text.contains("review the repository"));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_prompt_default_launch_stays_read_write_without_tmpfs_or_prompt_line() {
    let s = support::sandbox("command::read_only_workspace::exec_prompt_default_launch_stays_read_write_without_tmpfs_or_prompt_line");
    let f = fixture(&s, "prompt-rw", ONE_STEP, true);
    let (_, events) = run_prompt(&f, WorkspaceAccess::ReadWrite, false, None).await;
    let launches = f.launches();
    assert_eq!(launches.len(), 1, "{events}");
    let launch = &launches[0];
    assert!(!launch.read_only_workspace);
    assert_eq!(
        launch.permission("/workspace"),
        Some(OverlayPermission::ReadWrite)
    );
    assert!(launch.tmpfs.is_empty());
    assert!(!launch.prompt_text.contains(PROMPT_LINE));
    // No read-only status message; the policy report says read_write.
    assert!(
        !events.contains("\"phase\":\"workspace_access\""),
        "{events}"
    );
    assert!(
        events.contains("\\\"workspace_access\\\":\\\"read_write\\\""),
        "{events}"
    );
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_prompt_is_refused_before_any_effect_on_a_runtime_that_cannot_mount_read_only() {
    let s = support::sandbox("command::read_only_workspace::exec_prompt_is_refused_before_any_effect_on_a_runtime_that_cannot_mount_read_only");
    let f = fixture(&s, "prompt-sbx", ONE_STEP, false);
    let output = s.root().join("never-created");
    let (result, events) =
        run_prompt(&f, WorkspaceAccess::ReadOnly, false, Some(output.clone())).await;
    let err = result.expect_err("a read-only run on sbx must be refused");
    assert!(
        matches!(err, CommandError::ReadOnlyWorkspaceUnsupported { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains(REFUSAL), "{err}");
    assert_eq!(err.code(), "read_only_workspace_unsupported");
    assert_eq!(f.builds(), 0, "a refused run launched: {events}");
    assert!(!output.exists(), "preflight must precede the output dir");
    // The same runtime still runs an ordinary (writable) prompt.
    let (_, _) = run_prompt(&f, WorkspaceAccess::ReadWrite, false, None).await;
    assert_eq!(f.builds(), 1);
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_prompt_refuses_allow_docker_and_never_launches_read_write() {
    let s = support::sandbox("command::read_only_workspace::exec_prompt_refuses_allow_docker_and_never_launches_read_write");
    let f = fixture(&s, "prompt-docker", ONE_STEP, true);
    let (result, _) = run_prompt(&f, WorkspaceAccess::ReadOnly, true, None).await;
    let err = result.expect_err("--allow-docker defeats the read-only mount");
    assert!(
        matches!(&err, CommandError::InvalidFlagValue { flag, .. } if flag == READ_ONLY_WORKSPACE_FLAG),
        "{err:?}"
    );
    assert_eq!(f.builds(), 0);
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_workflow_command_flag_makes_every_step_read_only() {
    let s = support::sandbox(
        "command::read_only_workspace::exec_workflow_command_flag_makes_every_step_read_only",
    );
    let f = fixture(&s, "wf-flag", ONE_STEP, true);
    let (_, events) = run_workflow(&f, WorkspaceAccess::ReadOnly, false, false).await;
    let launches = f.launches();
    assert!(!launches.is_empty(), "{events}");
    for launch in &launches {
        assert!(launch.workspace_is_read_only(), "{launch:?}");
        assert_eq!(launch.tmpfs, [PathBuf::from("/tmp")]);
        assert!(launch.prompt_text.contains(PROMPT_LINE), "{launch:?}");
    }
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_workflow_step_option_applies_to_that_step_without_the_command_flag() {
    let s = support::sandbox("command::read_only_workspace::exec_workflow_step_option_applies_to_that_step_without_the_command_flag");
    let marked = fixture(&s, "wf-step-marked", STEP_MARKED, true);
    let (_, events) = run_workflow(&marked, WorkspaceAccess::ReadWrite, false, false).await;
    let launches = marked.launches();
    assert!(!launches.is_empty(), "{events}");
    assert!(launches[0].workspace_is_read_only(), "{:?}", launches[0]);
    assert_eq!(launches[0].tmpfs, [PathBuf::from("/tmp")]);
    assert!(launches[0].prompt_text.contains(PROMPT_LINE));

    let plain = fixture(&s, "wf-step-plain", ONE_STEP, true);
    let (_, events) = run_workflow(&plain, WorkspaceAccess::ReadWrite, false, false).await;
    let launches = plain.launches();
    assert!(!launches.is_empty(), "{events}");
    assert_eq!(
        launches[0].permission("/workspace"),
        Some(OverlayPermission::ReadWrite)
    );
    assert!(launches[0].tmpfs.is_empty());
    assert!(!launches[0].prompt_text.contains(PROMPT_LINE));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_workflow_with_a_read_only_step_is_refused_up_front_on_a_runtime_that_cannot() {
    let s = support::sandbox("command::read_only_workspace::exec_workflow_with_a_read_only_step_is_refused_up_front_on_a_runtime_that_cannot");
    // Step 1 is writable and would run fine; step 2 needs read-only. The
    // whole run is refused before step 1 launches, not midway.
    let f = fixture(&s, "wf-late-ro-sbx", LATE_RO_STEP, false);
    let (result, events) = run_workflow(&f, WorkspaceAccess::ReadWrite, false, false).await;
    let err = result.expect_err("the read-only step cannot be honoured");
    assert!(err.to_string().contains(REFUSAL), "{err}\n{events}");
    assert_eq!(f.builds(), 0, "step 1 ran before the refusal: {events}");

    // Command-level flag on the same runtime: refused as well.
    let f = fixture(&s, "wf-flag-sbx", ONE_STEP, false);
    let (result, _) = run_workflow(&f, WorkspaceAccess::ReadOnly, false, false).await;
    assert!(matches!(
        result,
        Err(CommandError::ReadOnlyWorkspaceUnsupported { .. })
    ));
    assert_eq!(f.builds(), 0);
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_workflow_read_only_step_after_a_writable_step_is_accepted_on_a_capable_runtime() {
    let s = support::sandbox("command::read_only_workspace::exec_workflow_read_only_step_after_a_writable_step_is_accepted_on_a_capable_runtime");
    let f = fixture(&s, "wf-late-ro", LATE_RO_STEP, true);
    let (_, events) = run_workflow(&f, WorkspaceAccess::ReadWrite, false, false).await;
    let launches = f.launches();
    assert!(!launches.is_empty(), "{events}");
    // The first step to launch is the writable one.
    assert_eq!(
        launches[0].permission("/workspace"),
        Some(OverlayPermission::ReadWrite),
        "{:?}",
        launches[0]
    );
    assert!(!launches[0].prompt_text.contains(PROMPT_LINE));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_workflow_refuses_incompatible_combinations_before_launch() {
    let s = support::sandbox(
        "command::read_only_workspace::exec_workflow_refuses_incompatible_combinations_before_launch",
    );
    // --read-only-workspace with setup/teardown steps.
    let f = fixture(&s, "wf-setup", WITH_SETUP, true);
    let (result, _) = run_workflow(&f, WorkspaceAccess::ReadOnly, false, false).await;
    let err = result.expect_err("setup steps run with a writable workspace");
    assert!(err.to_string().contains("setup or teardown"), "{err}");
    assert_eq!(f.builds(), 0);

    // --read-only-workspace with --dynamic.
    let f = fixture(&s, "wf-dynamic", ONE_STEP, true);
    let (result, _) = run_workflow(&f, WorkspaceAccess::ReadOnly, false, true).await;
    let err = result.expect_err("--dynamic cannot be checked up front");
    assert!(err.to_string().contains("--dynamic"), "{err}");
    assert_eq!(f.builds(), 0);

    // --read-only-workspace with --allow-docker.
    let f = fixture(&s, "wf-docker", ONE_STEP, true);
    let (result, _) = run_workflow(&f, WorkspaceAccess::ReadOnly, true, false).await;
    assert!(matches!(
        result,
        Err(CommandError::InvalidFlagValue { ref flag, .. }) if flag == READ_ONLY_WORKSPACE_FLAG
    ));
    assert_eq!(f.builds(), 0);
    s.finish();
}

/// The CLI (clap) and the API / TUI command box (raw-args projection) read the
/// same catalogue: the option is accepted on both exec leaves and their remote
/// forms, and refused in combination with `--allow-docker` by both.
#[test]
fn catalogue_projection_accepts_the_option_on_every_exec_path_and_refuses_allow_docker() {
    let catalogue = CommandCatalogue::get();
    let tokens = |list: &[&str]| list.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    for path in [
        &["exec", "prompt"][..],
        &["exec", "workflow"][..],
        &["remote", "exec", "prompt"][..],
        &["remote", "exec", "workflow"][..],
    ] {
        let parsed = catalogue
            .parse_raw_args(path, &tokens(&["--read-only-workspace"]))
            .unwrap_or_else(|e| panic!("{path:?}: {e:?}"));
        assert_eq!(
            parsed.flag_bool(READ_ONLY_WORKSPACE_FLAG),
            Some(true),
            "{path:?}"
        );
        assert!(
            catalogue
                .parse_raw_args(path, &tokens(&["--read-only-workspace", "--allow-docker"]))
                .is_err(),
            "{path:?} accepted both flags"
        );
    }
    // WI 0070: offered on chat too, with the same --allow-docker refusal.
    assert!(catalogue
        .parse_raw_args(&["chat"], &tokens(&["--read-only-workspace"]))
        .is_ok());
    assert!(catalogue
        .parse_raw_args(
            &["chat"],
            &tokens(&["--read-only-workspace", "--allow-docker"])
        )
        .is_err());
}

fn chat_flags(allow_docker: bool) -> ChatCommandFlags {
    ChatCommandFlags {
        non_interactive: false,
        plan: false,
        allow_docker,
        yolo: false,
        yolo_authorized: false,
        auto: false,
        auto_explicit: false,
        agent: Some("claude".into()),
        model: None,
        launch_mode: None,
        overlay: vec![],
        agent_flags: None,
        session_worktree: None,
        automation: None,
    }
}

async fn run_chat(
    f: &Fixture,
    access: WorkspaceAccess,
    allow_docker: bool,
) -> (Result<(), CommandError>, String) {
    let bus = EventBus::new(1024);
    let mut log = bus.subscribe_log();
    let frontend = ApiDispatchFrontend::new("chat", &[], bus.sender());
    let result = ChatCommand::new(
        chat_flags(allow_docker),
        f.engines.clone(),
        f.session.clone(),
    )
    .with_workspace_access(access)
    .run_with_frontend(Box::new(frontend))
    .await
    .map(|_| ());
    (result, drain(&mut log))
}

/// WI 0070: `chat --read-only-workspace` mirrors `exec prompt`: read-only
/// mount, scratch tmpfs, the prompt section, and no native plan tokens.
#[tokio::test(flavor = "multi_thread")]
async fn chat_read_only_launch_mounts_ro_tells_the_agent_and_adds_no_plan_tokens() {
    let s = support::sandbox("command::read_only_workspace::chat_read_only_launch_mounts_ro_tells_the_agent_and_adds_no_plan_tokens");
    let f = fixture(&s, "chat-ro", ONE_STEP, true);
    let (_, events) = run_chat(&f, WorkspaceAccess::ReadOnly, false).await;
    let launches = f.launches();
    assert_eq!(launches.len(), 1, "{events}");
    let launch = &launches[0];
    assert!(launch.read_only_workspace);
    assert!(launch.workspace_is_read_only(), "{launch:?}");
    assert_eq!(launch.tmpfs, [PathBuf::from("/tmp")]);
    assert_eq!(launch.tmpdir.as_deref(), Some("/tmp"));
    assert!(launch.prompt_text.contains(PROMPT_LINE), "{launch:?}");
    assert!(launch.mode_flags.is_empty(), "{:?}", launch.mode_flags);
    assert!(
        events.contains("\\\"workspace_access\\\":\\\"read_only\\\""),
        "{events}"
    );
    assert!(
        events.contains("\\\"native_start_mode\\\":\\\"default\\\""),
        "{events}"
    );

    // The default chat stays writable with no section.
    let (_, _) = run_chat(&f, WorkspaceAccess::ReadWrite, false).await;
    let launch = f.launches().pop().unwrap();
    assert!(!launch.read_only_workspace);
    assert_eq!(
        launch.permission("/workspace"),
        Some(OverlayPermission::ReadWrite)
    );
    assert!(!launch.prompt_text.contains(PROMPT_LINE));
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_read_only_is_refused_before_any_effect_where_it_cannot_be_enforced() {
    let s = support::sandbox("command::read_only_workspace::chat_read_only_is_refused_before_any_effect_where_it_cannot_be_enforced");
    let f = fixture(&s, "chat-sbx", ONE_STEP, false);
    let (result, events) = run_chat(&f, WorkspaceAccess::ReadOnly, false).await;
    let err = result.expect_err("refused on a runtime without read-only mounts");
    assert_eq!(err.code(), "read_only_workspace_unsupported", "{err}");
    assert_eq!(f.builds(), 0, "{events}");

    let f = fixture(&s, "chat-docker", ONE_STEP, true);
    let (result, _) = run_chat(&f, WorkspaceAccess::ReadOnly, true).await;
    assert!(
        matches!(&result, Err(CommandError::InvalidFlagValue { flag, .. }) if flag == READ_ONLY_WORKSPACE_FLAG),
        "{result:?}"
    );
    assert_eq!(f.builds(), 0);
    s.finish();
}

#[test]
fn chat_declares_read_only_workspace_and_no_implication_with_plan() {
    let catalogue = CommandCatalogue::get();
    let spec = catalogue.lookup(&["chat"]).unwrap();
    let flag = spec.find_flag(READ_ONLY_WORKSPACE_FLAG).unwrap();
    assert!(flag.implies.is_empty());
    let plan = spec.find_flag("plan").unwrap();
    assert!(plan.implies.is_empty());
    assert!(!plan.conflicts_with.contains(&READ_ONLY_WORKSPACE_FLAG));
    assert!(!catalogue.is_pending_flag(&["chat"], READ_ONLY_WORKSPACE_FLAG));
    catalogue
        .parse_raw_args(
            &["chat"],
            &["--plan".into(), "--read-only-workspace".into()],
        )
        .unwrap();
}
