//! WI 0070: `--plan` (native starting mode) and `--read-only-workspace`
//! (mount access) are independent on every launch route, proven through
//! production dispatch and commands against a recording runtime.
//!
//! The runtime records the resolved options the command layer hands it (the
//! only thing a real runtime consumes) and fails the build, so nothing is
//! launched. A "direct shell write" is replayed against the recorded mount
//! table: the same rule a container applies to a write under `/workspace`.
//! Real provider enforcement and in-session transitions are the boundary
//! tier's job (`tests/boundary/*`); none is claimed here.
use crate::support;
use awman::command::{
    commands::{
        api_server::event_bus::EventBus,
        chat::{ChatCommand, ChatCommandFlags},
        Command,
    },
    dispatch::{Dispatch, Engines},
    error::CommandError,
};
use awman::data::{
    fs::{ApiPaths, AuthPathResolver},
    session::{AgentHandle, Session, SessionOpenOptions},
    workspace::WorkspaceAccess,
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
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// What one launch asked of the runtime.
#[derive(Debug, Clone)]
struct Launch {
    /// Entrypoint, mode flags, then user agent flags: the provider argv.
    argv: Vec<String>,
    /// `(guest path, permission, host path)` of every mount.
    mounts: Vec<(String, OverlayPermission, PathBuf)>,
    tmpfs: Vec<PathBuf>,
    read_only_workspace: bool,
    env_passthrough: Vec<String>,
    /// The private OpenCode config copy, read while the launch owns it.
    private_opencode_config: Option<String>,
}

impl Launch {
    fn has(&self, tokens: &[&str]) -> bool {
        self.argv
            .windows(tokens.len())
            .any(|w| w.iter().zip(tokens).all(|(a, b)| a == b))
    }

    /// Replay `sh -c 'echo x > <guest>'` against the mount table: the most
    /// specific mount containing the path decides; a read-only mount fails
    /// like EROFS, a writable one writes through to its host directory.
    fn shell_write(&self, guest: &str) -> Result<PathBuf, String> {
        let (mount, permission, host) = self
            .mounts
            .iter()
            .filter(|(path, _, _)| Path::new(guest).starts_with(path))
            .max_by_key(|(path, _, _)| path.len())
            .ok_or_else(|| format!("{guest}: no such file or directory"))?;
        if *permission == OverlayPermission::ReadOnly {
            return Err(format!("{guest}: Read-only file system"));
        }
        let target = host.join(Path::new(guest).strip_prefix(mount).unwrap());
        std::fs::write(&target, "written by the agent\n").map_err(|e| e.to_string())?;
        Ok(target)
    }
}

struct Runtime {
    caps: Capabilities,
    launches: Mutex<Vec<Launch>>,
}

fn capabilities(container: bool) -> Capabilities {
    Capabilities {
        arbitrary_host_mounts: container,
        kit_declarative: !container,
        has_image_store: container,
        image_acquisition: if container {
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
        panic!("no image build is expected")
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
        let ResolvedAgentOptions::Container(options) = options else {
            panic!("a sandbox launch must be refused before provisioning in these tests")
        };
        let mut argv = options.entrypoint.clone().map(|e| e.0).unwrap_or_default();
        argv.extend(options.agent_mode_flags.iter().cloned());
        argv.extend(options.agent_flags.iter().cloned());
        let private_opencode_config = options
            .overlays
            .iter()
            .find(|o| o.container_path.ends_with(".config/opencode"))
            .and_then(|o| {
                ["opencode.json", "opencode.jsonc"]
                    .iter()
                    .find_map(|f| std::fs::read_to_string(o.host_path.join(f)).ok())
            });
        self.launches.lock().unwrap().push(Launch {
            argv,
            mounts: options
                .overlays
                .iter()
                .map(|o| {
                    (
                        o.container_path.display().to_string(),
                        o.permission,
                        o.host_path.clone(),
                    )
                })
                .collect(),
            tmpfs: options.tmpfs.clone(),
            read_only_workspace: options.read_only_workspace,
            env_passthrough: options
                .env_passthrough
                .iter()
                .map(|v| v.0.clone())
                .collect(),
            private_opencode_config,
        });
        Err(EngineError::Other("captured by test runtime".into()))
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
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
    fn last(&self) -> Launch {
        self.launches().pop().expect("one launch")
    }
}

fn fixture(s: &TestSandbox, name: &str, global: Value, workflow: &str, container: bool) -> Fixture {
    let repo = RepoBuilder::new(s, name)
        .file("README.md", "fixture\n")
        .file("workflow.toml", workflow)
        .init();
    let mut config = json!({"agents": {"auth": {"refresh": {"enabled": false}}}});
    merge(&mut config, global);
    ConfigTree::new().global(config).write(s, Some(repo.path()));
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
        caps: capabilities(container),
        launches: Default::default(),
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
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime.clone())),
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

fn merge(base: &mut Value, extra: Value) {
    match (base, extra) {
        (Value::Object(base), Value::Object(extra)) => {
            for (key, value) in extra {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, extra) => *base = extra,
    }
}

const ONE_STEP: &str = "title = 'one'\n\n[[step]]\nname = 'review'\nprompt = 'look around'\n";

fn drain(log: &mut tokio::sync::mpsc::UnboundedReceiver<impl serde::Serialize>) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(event) = log.try_recv() {
        events.push(serde_json::to_value(&event).unwrap());
    }
    events
}

/// The `agent_policy` reports in an event log, as JSON objects.
fn policies(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|e| {
            let text = e.to_string();
            if !text.contains("agent_policy") {
                return None;
            }
            let message = find_key(e, "message")?;
            serde_json::from_str::<Value>(message.as_str()?).ok()
        })
        .collect()
}

fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find_key(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find_key(v, key)),
        _ => None,
    }
}

/// Run `subcommand` through production dispatch with the daemon API
/// frontend (raw args, API profile defaults such as the unattended `--yolo`).
async fn dispatch(
    f: &Fixture,
    path: &[&str],
    args: &[&str],
) -> (Result<(), CommandError>, Vec<Value>) {
    let bus = EventBus::new(1024);
    let mut log = bus.subscribe_log();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let frontend = ApiDispatchFrontend::new(&path.join(" "), &args, bus.sender());
    let result = Dispatch::new(
        frontend,
        Arc::new(tokio::sync::RwLock::new(f.session.clone())),
        f.engines.clone(),
    )
    .run_command(path)
    .await
    .map(|_| ());
    (result, drain(&mut log))
}

fn chat_flags(agent: &str, plan: bool) -> ChatCommandFlags {
    ChatCommandFlags {
        non_interactive: false,
        plan,
        allow_docker: false,
        yolo: false,
        yolo_authorized: false,
        auto: false,
        auto_explicit: false,
        agent: Some(agent.into()),
        model: None,
        launch_mode: None,
        overlay: vec![],
        agent_flags: None,
        session_worktree: None,
        automation: None,
    }
}

async fn chat(
    f: &Fixture,
    flags: ChatCommandFlags,
    access: WorkspaceAccess,
) -> (Result<(), CommandError>, Vec<Value>) {
    let bus = EventBus::new(1024);
    let mut log = bus.subscribe_log();
    let frontend = ApiDispatchFrontend::new("chat", &[], bus.sender());
    let result = ChatCommand::new(flags, f.engines.clone(), f.session.clone())
        .with_workspace_access(access)
        .run_with_frontend(Box::new(frontend))
        .await
        .map(|_| ());
    (result, drain(&mut log))
}

fn sandbox(name: &'static str) -> TestSandbox {
    support::sandbox(name)
}

/// The four combinations on `exec prompt` through the API route: argv and
/// mount access are asserted separately, and a direct shell write follows
/// the mount, never the native mode.
#[tokio::test(flavor = "multi_thread")]
async fn exec_prompt_four_combinations_keep_argv_and_mounts_independent() {
    let s = sandbox(
        "command::native_plan::exec_prompt_four_combinations_keep_argv_and_mounts_independent",
    );
    let f = fixture(&s, "four-combinations", json!({}), ONE_STEP, true);
    for (plan, read_only) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut args = vec!["--agent", "claude"];
        if plan {
            args.push("--plan");
        }
        if read_only {
            args.push("--read-only-workspace");
        }
        args.push("review");
        let before = f.launches().len();
        let (result, events) = dispatch(&f, &["exec", "prompt"], &args).await;
        assert!(result.is_err(), "the recording runtime never launches");
        assert_eq!(f.launches().len(), before + 1, "plan={plan} ro={read_only}");
        let launch = f.last();

        // Native mode half: argv only.
        assert_eq!(
            launch.has(&["--permission-mode", "plan"]),
            plan,
            "{:?}",
            launch.argv
        );
        // The API's unattended --yolo default is suppressed by --plan only.
        assert_eq!(
            launch.has(&["--dangerously-skip-permissions"]),
            !plan,
            "{:?}",
            launch.argv
        );
        // Mount half: mounts only.
        assert_eq!(launch.read_only_workspace, read_only);
        let workspace = launch
            .mounts
            .iter()
            .find(|(p, _, _)| p == "/workspace")
            .unwrap();
        assert_eq!(workspace.1 == OverlayPermission::ReadOnly, read_only);
        assert_eq!(
            launch.tmpfs.iter().any(|p| p == Path::new("/tmp")),
            read_only,
            "plan alone adds no scratch tmpfs"
        );
        // A direct shell write, whatever mode the agent is in.
        let write = launch.shell_write("/workspace/NOTES.md");
        assert_eq!(write.is_ok(), !read_only, "{write:?}");
        if let Ok(host) = write {
            assert!(host.starts_with(f.session.git_root()));
            std::fs::remove_file(host).unwrap();
        }
        // Reported separately.
        let reported = policies(&events);
        assert_eq!(reported.len(), 1, "{events:#?}");
        assert_eq!(
            reported[0]["native_start_mode"],
            if plan { "plan" } else { "default" }
        );
        assert_eq!(
            reported[0]["workspace_access"],
            if read_only { "read_only" } else { "read_write" }
        );
    }
    s.finish();
}

/// Option level only: a chat `--plan` launch asks the runtime for exactly
/// one container whose mount table alone decides whether a shell write to
/// `/workspace` can succeed (replayed against the recorded mounts; nothing
/// runs). `--plan` contributes no mount restriction. The final `docker run`
/// argv and a completed launch are proven by
/// `parity::builtin::plan_and_mount_access_reach_the_final_launch_independently`;
/// ACP mode selection and acknowledgement by `engine::acp::session::start_mode_tests`;
/// an in-session provider transition is boundary-tier evidence.
#[tokio::test(flavor = "multi_thread")]
async fn plan_launch_options_leave_workspace_writes_to_the_mount_flag() {
    let s = sandbox(
        "command::native_plan::plan_launch_options_leave_workspace_writes_to_the_mount_flag",
    );
    let f = fixture(&s, "transition", json!({}), ONE_STEP, true);
    for read_only in [false, true] {
        let access = WorkspaceAccess::from_read_only(read_only);
        let before = f.launches().len();
        let (result, _) = chat(&f, chat_flags("codex", true), access).await;
        let err = result.expect_err("the recording runtime never starts a container");
        assert!(
            err.to_string().contains("captured by test runtime"),
            "{err}"
        );
        let launches = f.launches();
        assert_eq!(
            launches.len(),
            before + 1,
            "exactly one container per session"
        );
        let launch = launches.last().unwrap();
        assert!(
            launch.has(&["codex", "--no-daemon", "--sandbox", "read-only"]),
            "{:?}",
            launch.argv
        );
        // Whatever mode the provider is in, a shell write follows the mount.
        let edit = launch.shell_write("/workspace/src.rs");
        assert_eq!(edit.is_err(), read_only, "{edit:?}");
        if let Ok(host) = edit {
            std::fs::remove_file(host).unwrap();
        }
    }
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_plan_maps_each_agent_without_touching_mounts() {
    let s = sandbox("command::native_plan::chat_plan_maps_each_agent_without_touching_mounts");
    let f = fixture(&s, "chat-mapping", json!({}), ONE_STEP, true);
    for (agent, tokens) in [
        ("claude", &["--permission-mode", "plan"][..]),
        ("codex", &["--sandbox", "read-only"][..]),
        ("opencode", &["--agent", "plan"][..]),
        ("cursor", &["--mode", "plan"][..]),
        ("copilot", &["--plan"][..]),
        ("cline", &["--plan"][..]),
        ("antigravity", &["--mode", "plan"][..]),
    ] {
        let (_, events) = chat(&f, chat_flags(agent, true), WorkspaceAccess::ReadWrite).await;
        let launch = f.last();
        assert!(launch.has(tokens), "{agent}: {:?}", launch.argv);
        assert!(!launch.read_only_workspace, "{agent}");
        let reported = policies(&events);
        assert_eq!(reported[0]["workspace_access"], "read_write", "{agent}");
    }
    // OpenCode 2's full TUI: a private default_agent, no argv, host untouched.
    // Its private config copy is unix-only.
    #[cfg(unix)]
    {
        let host = s.home().join(".config/opencode");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("opencode.json"), "{\"default_agent\": \"build\"}").unwrap();
        let (_, _) = chat(
            &f,
            chat_flags("opencode-v2", true),
            WorkspaceAccess::ReadWrite,
        )
        .await;
        let launch = f.last();
        assert_eq!(launch.argv, ["opencode", "--standalone"]);
        let private: Value =
            serde_json::from_str(launch.private_opencode_config.as_deref().unwrap()).unwrap();
        assert_eq!(private["default_agent"], "plan");
        assert_eq!(
            std::fs::read_to_string(host.join("opencode.json")).unwrap(),
            "{\"default_agent\": \"build\"}"
        );
    }
    s.finish();
}

/// Contradicting provider selectors are refused before any launch, from
/// `--agent-flag` and from `agents.flags`; a matching duplicate is dropped.
#[tokio::test(flavor = "multi_thread")]
async fn agent_mode_conflicts_are_refused_before_launch() {
    let s = sandbox("command::native_plan::agent_mode_conflicts_are_refused_before_launch");
    let f = fixture(
        &s,
        "conflicts",
        json!({"agents": {"flags": {"codex": ["--sandbox", "workspace-write"]}}}),
        ONE_STEP,
        true,
    );
    let (result, _) = dispatch(
        &f,
        &["exec", "prompt"],
        &[
            "--agent",
            "claude",
            "--plan",
            "--agent-flag",
            "--permission-mode=acceptEdits",
            "review",
        ],
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(
        (err.code(), err.http_status(), err.exit_code()),
        ("agent_mode_conflict", 400, 2),
        "{err}"
    );
    let (result, _) = dispatch(
        &f,
        &["exec", "prompt"],
        &["--agent", "codex", "--plan", "review"],
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.code(), "agent_mode_conflict", "{err}");
    assert!(err.to_string().contains("agents.flags"), "{err}");
    assert!(f.launches().is_empty(), "nothing launched");
    // Without --plan the configured flag is ordinary.
    let _ = dispatch(&f, &["exec", "prompt"], &["--agent", "codex", "review"]).await;
    assert!(f.last().has(&["--sandbox", "workspace-write"]));
    // A duplicate of the plan mapping is normalised once.
    let _ = dispatch(
        &f,
        &["exec", "prompt"],
        &[
            "--agent",
            "claude",
            "--plan",
            "--agent-flag",
            "--permission-mode=plan",
            "review",
        ],
    )
    .await;
    let launch = f.last();
    assert_eq!(
        launch.argv.iter().filter(|a| a.as_str() == "plan").count(),
        1,
        "{:?}",
        launch.argv
    );
    s.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_yolo_or_auto_with_plan_is_invalid_usage() {
    let s = sandbox("command::native_plan::explicit_yolo_or_auto_with_plan_is_invalid_usage");
    let f = fixture(&s, "explicit-modes", json!({}), ONE_STEP, true);
    for mode in ["--yolo", "--auto"] {
        let (result, _) = dispatch(
            &f,
            &["exec", "prompt"],
            &["--agent", "claude", "--plan", mode, "review"],
        )
        .await;
        let err = result.unwrap_err();
        assert_eq!(
            (err.exit_code(), err.http_status()),
            (2, 400),
            "{mode}: {err}"
        );
    }
    assert!(f.launches().is_empty());
    s.finish();
}

/// OpenCode's environment permission default is not passed with --plan
/// (and reported); the host is unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn plan_drops_an_inherited_opencode_permission_default() {
    let s = sandbox("command::native_plan::plan_drops_an_inherited_opencode_permission_default");
    let f = fixture(&s, "env-drop", json!({}), ONE_STEP, true);
    let (_, events) = dispatch(
        &f,
        &["exec", "prompt"],
        &[
            "--agent",
            "opencode",
            "--plan",
            "--overlay",
            "env(OPENCODE_PERMISSION)",
            "review",
        ],
    )
    .await;
    let launch = f.last();
    assert!(!launch
        .env_passthrough
        .contains(&"OPENCODE_PERMISSION".to_string()));
    assert!(launch.has(&["run", "--agent", "plan"]), "{:?}", launch.argv);
    assert!(
        events.iter().any(|e| e
            .to_string()
            .contains("OPENCODE_PERMISSION is not passed to a --plan launch")),
        "{events:#?}"
    );
    let _ = dispatch(
        &f,
        &["exec", "prompt"],
        &[
            "--agent",
            "opencode",
            "--overlay",
            "env(OPENCODE_PERMISSION)",
            "review",
        ],
    )
    .await;
    assert!(f
        .last()
        .env_passthrough
        .contains(&"OPENCODE_PERMISSION".to_string()));
    s.finish();
}

/// A pulled lease's read-only engine is read-only whatever --plan says, and
/// --plan still selects only the native mode.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_lease_engine_and_plan_resolve_separately() {
    let s = sandbox("command::native_plan::a_read_only_lease_engine_and_plan_resolve_separately");
    let mut f = fixture(&s, "lease", json!({}), ONE_STEP, true);
    f.engines.agent_engine = Arc::new(
        (*f.engines.agent_engine)
            .clone()
            .with_read_only_workspace(true),
    );
    for plan in [false, true] {
        let mut args = vec!["--agent", "codex"];
        if plan {
            args.push("--plan");
        }
        args.push("review");
        let (_, events) = dispatch(&f, &["exec", "prompt"], &args).await;
        let launch = f.last();
        assert!(launch.read_only_workspace);
        assert_eq!(
            launch.has(&["--sandbox", "read-only"]),
            plan,
            "{:?}",
            launch.argv
        );
        let reported = policies(&events);
        assert_eq!(reported[0]["workspace_access"], "read_only");
        assert_eq!(
            reported[0]["native_start_mode"],
            if plan { "read_only" } else { "default" }
        );
    }
    s.finish();
}

/// Workflow steps: the command flag selects the native mode of every agent
/// step; a step that cannot deliver it fails the whole run before step 1.
#[tokio::test(flavor = "multi_thread")]
async fn exec_workflow_plan_applies_per_step_and_refuses_up_front() {
    let s =
        sandbox("command::native_plan::exec_workflow_plan_applies_per_step_and_refuses_up_front");
    let f = fixture(&s, "workflow-plan", json!({}), ONE_STEP, true);
    let (_, events) = dispatch(
        &f,
        &["exec", "workflow"],
        &["workflow.toml", "--agent", "cursor", "--plan", "--headless"],
    )
    .await;
    let launch = f.last();
    assert!(launch.has(&["--mode", "plan"]), "{:?}", launch.argv);
    assert!(
        !launch.has(&["--force"]),
        "no implied yolo with --plan: {:?}",
        launch.argv
    );
    assert!(!launch.read_only_workspace);
    let reported = policies(&events);
    assert_eq!(reported.last().unwrap()["step"], "review", "{events:#?}");
    assert_eq!(reported.last().unwrap()["native_start_mode"], "plan");

    s.finish();
}

/// Docker Sandboxes: a mixin without a managed plan route (codex) is
/// refused before provisioning; mount-only stays refused independently.
#[tokio::test(flavor = "multi_thread")]
async fn sandbox_runtime_refuses_an_undeliverable_plan_before_provisioning() {
    let s = sandbox(
        "command::native_plan::sandbox_runtime_refuses_an_undeliverable_plan_before_provisioning",
    );
    let f = fixture(&s, "sandbox", json!({}), ONE_STEP, false);
    let (result, _) = dispatch(
        &f,
        &["exec", "prompt"],
        &["--agent", "codex", "--plan", "review"],
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(
        (err.code(), err.http_status()),
        ("native_capability_unavailable", 409),
        "{err}"
    );
    let (result, _) = dispatch(
        &f,
        &["exec", "workflow"],
        &[
            "workflow.toml",
            "--agent",
            "copilot",
            "--plan",
            "--headless",
        ],
    )
    .await;
    assert_eq!(result.unwrap_err().code(), "native_capability_unavailable");
    let (result, _) = dispatch(
        &f,
        &["exec", "prompt"],
        &["--agent", "claude", "--read-only-workspace", "review"],
    )
    .await;
    assert_eq!(
        result.unwrap_err().code(),
        "read_only_workspace_unsupported"
    );
    assert!(
        f.launches().is_empty(),
        "the runtime was never asked to build"
    );
    s.finish();
}

/// WI 0070 review finding 2: OpenCode's `.opencode/opencode.json[c]` is
/// project configuration that outranks the direct file. A v2 full-TUI chat
/// whose only conflicting default lives there is refused before launch.
#[tokio::test(flavor = "multi_thread")]
async fn a_nested_opencode_project_default_is_refused_before_launch() {
    let s =
        sandbox("command::native_plan::a_nested_opencode_project_default_is_refused_before_launch");
    let f = fixture(&s, "nested-opencode", json!({}), ONE_STEP, true);
    let nested = f.session.git_root().join(".opencode");
    std::fs::create_dir_all(&nested).unwrap();
    for name in ["opencode.json", "opencode.jsonc"] {
        std::fs::write(nested.join(name), "{\"default_agent\": \"build\"}").unwrap();
        let (result, _) = chat(
            &f,
            chat_flags("opencode-v2", true),
            WorkspaceAccess::ReadWrite,
        )
        .await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), "agent_mode_conflict", "{name}: {err}");
        assert!(err.to_string().contains(".opencode"), "{name}: {err}");
        std::fs::remove_file(nested.join(name)).unwrap();
    }
    assert!(f.launches().is_empty(), "nothing launched");
    s.finish();
}

/// WI 0070 review finding 4: an overlay that replaces the OpenCode
/// configuration the guard checked is refused through production dispatch,
/// with and without the read-only workspace; the same overlay without
/// `--plan` is an ordinary mount.
#[tokio::test(flavor = "multi_thread")]
async fn an_overlay_replacing_checked_opencode_config_is_refused() {
    let s =
        sandbox("command::native_plan::an_overlay_replacing_checked_opencode_config_is_refused");
    let f = fixture(&s, "overlay-config", json!({}), ONE_STEP, true);
    let payload = s.root().join("payload.json");
    std::fs::write(&payload, "{\"agent\": {\"plan\": {\"disable\": true}}}").unwrap();
    let overlay = format!("dir({}:/workspace/opencode.json:ro)", payload.display());
    for extra in [
        &[][..],
        // A read-only OpenCode launch carries its instructions in a context
        // directory.
        &["--read-only-workspace", "--overlay", "context(global:ro)"][..],
    ] {
        let mut args = vec!["--agent", "opencode", "--plan", "--overlay", &overlay];
        args.extend_from_slice(extra);
        args.push("review");
        let (result, _) = dispatch(&f, &["exec", "prompt"], &args).await;
        let err = result.unwrap_err();
        assert_eq!(err.code(), "agent_mode_conflict", "{extra:?}: {err}");
        assert!(
            err.to_string().contains("/workspace/opencode.json"),
            "{err}"
        );
    }
    assert!(f.launches().is_empty(), "nothing launched");
    let (_, _) = dispatch(
        &f,
        &["exec", "prompt"],
        &["--agent", "opencode", "--overlay", &overlay, "review"],
    )
    .await;
    let launch = f.last();
    assert!(
        launch
            .mounts
            .iter()
            .any(|(guest, _, host)| guest == "/workspace/opencode.json" && host == &payload),
        "{:?}",
        launch.mounts
    );
    s.finish();
}
