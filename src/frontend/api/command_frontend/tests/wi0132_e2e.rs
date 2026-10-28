//! WI 0132 end-to-end: an API-mode dynamic workflow through the real
//! `ExecWorkflowCommand`, the API frontend, and the real `/v1/workflows/{id}`
//! route, with a container runtime that only records what it was asked to run.

use super::super::*;
use crate::command::commands::api_server::queue_worker::ApiCommandFrontendFactory;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::command::commands::api_server::event_bus::EventBus;
use crate::command::commands::command_trait::Command;
use crate::command::commands::exec_workflow::{ExecWorkflowCommand, ExecWorkflowCommandFlags};
use crate::command::dispatch::Engines;
use crate::data::config::repo::OnDirtyMergeTarget;
use crate::data::execution_event::{EventPayload, PhaseStatusKind};
use crate::data::fs::api_command_log::CommandStateWriter;
use crate::data::fs::api_db::{NewSessionRow, SqliteSessionStore};
use crate::data::fs::api_paths::ApiPaths;
use crate::data::session::{Session, SessionKind};
use crate::data::session_setup_event::SessionSetupStatus;
use crate::data::workflow_state::{PhaseStepStatus, StepState, WorkflowPhase, WorkflowState};
use crate::engine::git::GitEngine;
use crate::frontend::api::routes::{build_router, AppState, AuthMode};

const WORKFLOW: &str = r#"
[[setup]]
type = "run_shell"
command = "echo setup-ran"

[[steps]]
name = "generated"
agent = "claude"
prompt = "work"

[[teardown]]
type = "run_shell"
command = "echo teardown-ran"
"#;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The "server checkout": a committed repo with a work item and an agent
/// Dockerfile, so nothing is dirty before the run.
fn server_checkout(root: &Path) -> PathBuf {
    // Named after the temp dir, so each run gets its own `~/.awman/worktrees/<name>-<hash8>`.
    let repo = root.join(format!(
        "checkout-{}",
        root.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(repo.join(".awman")).unwrap();
    std::fs::create_dir_all(repo.join("aspec/work-items")).unwrap();
    std::fs::write(repo.join(".awman/Dockerfile.claude"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join(".awman/Dockerfile.codex"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join("aspec/work-items/0042-test.md"), "work item\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@awman.test"]);
    git(&repo, &["config", "user.name", "awman-test"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn app_state(root: &Path) -> Arc<AppState> {
    let paths = ApiPaths::from_root(root);
    paths.ensure_root().unwrap();
    let store = SqliteSessionStore::open(paths.root()).unwrap();
    Arc::new(AppState {
        store: Arc::new(store),
        paths: paths.clone(),
        workdirs: vec![],
        workdir_roots: Vec::new(),
        started_at: std::time::Instant::now(),
        task_handles: tokio::sync::Mutex::new(Vec::new()),
        auth_mode: AuthMode::Disabled,
        engines: Engines::for_tests(root),
        sessions: Arc::new(crate::data::session_manager::SessionManager::in_memory(
            crate::data::config::env::EnvSnapshot::empty(),
        )),
        event_buses: Arc::new(tokio::sync::Mutex::new(Default::default())),
        setup_buses: tokio::sync::Mutex::new(Default::default()),
    })
}

fn flags(on_dirty_merge_target: Option<OnDirtyMergeTarget>) -> ExecWorkflowCommandFlags {
    ExecWorkflowCommandFlags {
        workflow: None,
        work_item: Some("0042".into()),
        non_interactive: true,
        plan: false,
        allow_docker: false,
        worktree: false,
        yolo: false,
        auto: false,
        agent: None,
        model: None,
        launch_mode: None,
        overlay: vec![],
        agent_flags: None,
        max_concurrent: None,
        issue_source: crate::engine::issue::IssueSourceFlags { issue: None },
        dynamic: true,
        leader: None,
        on_dirty_merge_target,
        on_merge_failure: None,
        on_existing_worktree: None,
    }
}

/// Everything one run produced.
struct Run {
    result: Result<(), CommandError>,
    repo: PathBuf,
    worktree: PathBuf,
    commands: Vec<String>,
    /// `(phase, status)` of every setup/teardown transition event, in order.
    phase_events: Vec<(String, PhaseStatusKind)>,
    /// The snapshot served by `GET /v1/workflows/{id}` after the run.
    served: WorkflowState,
    /// Distinct snapshots a poller saw while the command ran, in order.
    polled: Vec<WorkflowState>,
}

/// Run `exec workflow --dynamic --work-item 42` the way the API queue worker
/// does. The teardown command stands in for the agent's work (writes
/// `generated.txt` into the worktree) and, when `dirty_during_run`, for a
/// person editing the server checkout while the run was in progress.
async fn run_api_dynamic_workflow(
    on_dirty_merge_target: Option<OnDirtyMergeTarget>,
    dirty_during_run: bool,
) -> (Run, tempfile::TempDir) {
    run_api_dynamic_workflow_with_initial_dirty(on_dirty_merge_target, dirty_during_run, false)
        .await
}

async fn run_api_dynamic_workflow_with_initial_dirty(
    on_dirty_merge_target: Option<OnDirtyMergeTarget>,
    dirty_during_run: bool,
    dirty_at_start: bool,
) -> (Run, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = server_checkout(tmp.path());
    if dirty_at_start {
        std::fs::write(repo.join("initial-draft.txt"), "initial user work\n").unwrap();
    }
    let worktree = GitEngine::new().worktree_path(&repo, 42).unwrap();

    let app = app_state(&tmp.path().join("api"));
    app.store
        .insert_session_full(NewSessionRow {
            id: "session",
            workdir: repo.to_str().unwrap(),
            created_at: &chrono::Utc::now().to_rfc3339(),
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    app.store
        .enqueue_command("command", "session", "exec workflow", "[]", "/log")
        .unwrap();
    std::fs::create_dir_all(app.paths.command_dir("session", "command")).unwrap();
    let router = build_router(app.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let endpoint = format!("http://{addr}/v1/workflows/command");

    let commands = Arc::new(Mutex::new(Vec::<String>::new()));
    let hook_commands = commands.clone();
    let (hook_repo, hook_worktree) = (repo.clone(), worktree.clone());
    let held = Arc::new(Mutex::new(Vec::new()));
    let runtime = crate::engine::container::test_support::holding_runtime_with_workflow(
        held,
        WORKFLOW,
        Arc::new(move |command: &str| {
            hook_commands.lock().unwrap().push(command.to_string());
            if command != "echo teardown-ran" {
                return;
            }
            std::fs::write(hook_worktree.join("generated.txt"), "agent work\n").unwrap();
            if dirty_during_run {
                std::fs::write(hook_repo.join("scratch.txt"), "edited meanwhile\n").unwrap();
            }
        }),
    );
    let mut engines = Engines::for_tests(&repo);
    engines.runtime = runtime.clone();
    engines.container_runtime = Some(runtime.clone());
    engines.agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        engines.overlay_engine.clone(),
        runtime,
    ));

    let bus = EventBus::new(256);
    let mut events = bus.subscribe();
    let frontend = crate::frontend::api::ApiFrontendFactory.frontend_for(
        &app.store.get_command("command").unwrap().unwrap(),
        bus.sender(),
        CommandStateWriter::new(app.paths.clone(), "session", "command"),
    );
    let session = Session::for_tests_isolated(&repo, &tmp.path().join("config-home"));

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = {
        let stop = stop.clone();
        let client = reqwest::Client::new();
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            let mut seen: Vec<WorkflowState> = Vec::new();
            loop {
                if let Ok(response) = client.get(&endpoint).send().await {
                    if response.status() == 200 {
                        if let Ok(state) = response.json::<WorkflowState>().await {
                            if seen.last() != Some(&state) {
                                seen.push(state);
                            }
                        }
                    }
                }
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return seen;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        ExecWorkflowCommand::new(flags(on_dirty_merge_target), engines, session)
            .run_with_frontend(Box::new(frontend)),
    )
    .await
    .expect("dynamic workflow timed out")
    .map(|_| ());

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let polled = poller.await.unwrap();
    let served: WorkflowState = reqwest::get(&endpoint).await.unwrap().json().await.unwrap();
    server.abort();

    let phase_events = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event.payload {
            EventPayload::WorkflowPhaseTransition { phase, status, .. }
                if phase == "setup" || phase == "teardown" =>
            {
                Some((phase, status))
            }
            _ => None,
        })
        .collect();
    let commands = commands.lock().unwrap().clone();
    (
        Run {
            result,
            repo,
            worktree,
            commands,
            phase_events,
            served,
            polled,
        },
        tmp,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_dynamic_workflow_runs_teardown_and_squash_merges() {
    let (run, _tmp) = run_api_dynamic_workflow(None, false).await;
    run.result.as_ref().unwrap_or_else(|e| panic!("{e}"));

    // The teardown step ran, between its Running and Succeeded events.
    assert_eq!(
        run.commands,
        vec![
            "echo setup-ran".to_string(),
            "echo teardown-ran".to_string()
        ]
    );
    assert_eq!(
        run.phase_events,
        vec![
            ("setup".to_string(), PhaseStatusKind::Running),
            ("setup".to_string(), PhaseStatusKind::Succeeded),
            ("teardown".to_string(), PhaseStatusKind::Running),
            ("teardown".to_string(), PhaseStatusKind::Succeeded),
        ]
    );

    // GET /v1/workflows/{id} shows the finished run.
    let state = &run.served;
    assert_eq!(state.current_phase, WorkflowPhase::Done);
    assert!(state.teardown_completed);
    assert_eq!(state.step_states["generated"], StepState::Succeeded);
    assert_eq!(state.teardown_step_states.len(), 1);
    assert_eq!(
        state.teardown_step_states[0].status,
        PhaseStepStatus::Succeeded
    );

    // The branch was squash-merged into the server checkout and cleaned up.
    assert_eq!(
        std::fs::read_to_string(run.repo.join("generated.txt")).unwrap(),
        "agent work\n"
    );
    assert!(git(&run.repo, &["log", "-1", "--format=%s"]).starts_with("Implement "));
    assert_eq!(git(&run.repo, &["status", "--porcelain"]), "");
    assert!(!run.worktree.exists(), "worktree removed after the merge");

    // What a client polling the route saw while the command ran: whole
    // snapshots of one invocation whose teardown status only moves forward
    // (Pending, then Running, then Succeeded) and which ends on the final one.
    assert!(!run.polled.is_empty());
    assert_eq!(run.polled.last(), Some(&run.served));
    let rank = |status: &PhaseStepStatus| match status {
        PhaseStepStatus::Pending => 0,
        PhaseStepStatus::Running => 1,
        _ => 2,
    };
    let phases: Vec<_> = run
        .polled
        .iter()
        .map(|s| match s.current_phase {
            WorkflowPhase::Setup => 0,
            WorkflowPhase::Main => 1,
            WorkflowPhase::Teardown => 2,
            WorkflowPhase::Done => 3,
        })
        .collect();
    assert!(phases.windows(2).all(|w| w[0] <= w[1]), "{phases:?}");
    let ranks: Vec<_> = run
        .polled
        .iter()
        .map(|s| rank(&s.teardown_step_states[0].status))
        .collect();
    assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{ranks:?}");
    assert!(run
        .polled
        .iter()
        .all(|s| s.invocation_id == run.served.invocation_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_dynamic_workflow_with_a_dirty_checkout_fails_with_the_dirty_target_error() {
    let (run, _tmp) = run_api_dynamic_workflow(None, true).await;
    let error = run
        .result
        .expect_err("a dirty merge target must fail the run");
    assert!(
        matches!(error, CommandError::MergeTargetDirty { .. }),
        "{error}"
    );
    // Not a merge conflict, and nothing was merged or lost.
    assert!(
        !error.to_string().to_lowercase().contains("conflict"),
        "{error}"
    );
    assert!(!run.repo.join("generated.txt").exists());
    assert_eq!(
        std::fs::read_to_string(run.repo.join("scratch.txt")).unwrap(),
        "edited meanwhile\n"
    );
    assert!(run.worktree.exists(), "worktree kept for the operator");
    assert_eq!(run.served.current_phase, WorkflowPhase::Done);
    assert!(run.served.teardown_completed);
    let _ = std::fs::remove_dir_all(&run.worktree);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_dynamic_workflow_with_a_dirty_checkout_and_stash_flag_merges() {
    let (run, _tmp) = run_api_dynamic_workflow(Some(OnDirtyMergeTarget::Stash), true).await;
    run.result.as_ref().unwrap_or_else(|e| panic!("{e}"));
    assert!(run.repo.join("generated.txt").exists(), "merged");
    assert!(!run.repo.join("scratch.txt").exists(), "stashed away");
    let stashes = git(&run.repo, &["stash", "list"]);
    assert_eq!(stashes.lines().count(), 1, "{stashes}");
    assert!(!run.worktree.exists());
    assert!(run.served.teardown_completed);
}

#[derive(Clone, Copy, Debug)]
enum RepairCase {
    Resolve,
    BuildFailure,
    ImageFailure,
    StartFailure,
    Unresolved,
    RejectedTemplate,
    OverrideIdentity,
}

/// Exercise the command's real finalize -> remediation factory -> runtime ->
/// verification path, including API output forwarding and parent checkout mount.
async fn run_merge_repair(case: RepairCase) {
    use crate::data::config::repo::OnMergeFailure;
    use crate::engine::container::options::ModelFlagForm;
    use crate::engine::error::EngineError;
    const PARALLEL: &str = r#"
[[steps]]
name = "a"
agent = "claude"
model = "alpha"
prompt = "work"
[[steps]]
name = "b"
agent = "claude"
model = "beta"
prompt = "work"
[[teardown]]
type = "run_shell"
command = "make-conflict"
"#;
    let tmp = tempfile::tempdir().unwrap();
    let repo = server_checkout(tmp.path());
    std::fs::write(
        repo.join("conflict-{{work_item_content}}-```.txt"),
        "base\n",
    )
    .unwrap();
    if matches!(case, RepairCase::RejectedTemplate) {
        std::fs::write(tmp.path().join("secret.md"), "OUTSIDE_SENTINEL").unwrap();
        std::fs::write(
            repo.join(".awman/config.json"),
            r#"{"worktree":{"mergeFix":{"promptTemplate":"../secret.md"}}}"#,
        )
        .unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "conflict base"]);
    let worktree = GitEngine::new().worktree_path(&repo, 42).unwrap();
    let bus = EventBus::new(1024);
    let mut events = bus.subscribe();
    let app = app_state(&tmp.path().join("api"));
    app.store
        .insert_session_full(NewSessionRow {
            id: "s",
            workdir: repo.to_str().unwrap(),
            created_at: "now",
            setup_status: SessionSetupStatus::Ready,
            kind: SessionKind::Local,
            cloned_path: None,
        })
        .unwrap();
    app.store
        .enqueue_command("c", "s", "exec workflow", "[]", "/log")
        .unwrap();
    std::fs::create_dir_all(app.paths.command_dir("s", "c")).unwrap();
    let frontend = crate::frontend::api::ApiFrontendFactory.frontend_for(
        &app.store.get_command("c").unwrap().unwrap(),
        bus.sender(),
        CommandStateWriter::new(app.paths.clone(), "s", "c"),
    );

    let calls = Arc::new(Mutex::new(Vec::<(String, Option<ModelFlagForm>)>::new()));
    let (hook_repo, hook_worktree) = (repo.clone(), worktree.clone());
    let build_calls = calls.clone();
    let build_repo = repo.clone();
    let held = Arc::new(Mutex::new(Vec::new()));
    let starting_repair = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let build_starting = starting_repair.clone();
    let runtime = crate::engine::container::test_support::holding_runtime_with_hooks(
        held.clone(),
        PARALLEL,
        Arc::new(move |_| {
            std::fs::write(
                hook_repo.join("conflict-{{work_item_content}}-```.txt"),
                "main\n",
            )
            .unwrap();
            git(&hook_repo, &["add", "."]);
            git(&hook_repo, &["commit", "-qm", "main change"]);
            std::fs::write(
                hook_worktree.join("conflict-{{work_item_content}}-```.txt"),
                "feature\n",
            )
            .unwrap();
            std::fs::write(hook_worktree.join("feature-only.txt"), "keep this\n").unwrap();
        }),
        Arc::new(move |options| {
            let prompt = options.seeded_prompt.clone().unwrap_or_default();
            if !prompt.contains("# Fix a failed merge") {
                return Ok(());
            }
            build_starting.store(true, std::sync::atomic::Ordering::SeqCst);
            build_calls
                .lock()
                .unwrap()
                .push((prompt.clone(), options.model.clone()));
            assert!(!options.interactive, "API repair runs non-interactively");
            let expected_agent = if matches!(case, RepairCase::OverrideIdentity) {
                "codex"
            } else {
                "claude"
            };
            assert!(options
                .image
                .as_ref()
                .unwrap()
                .as_str()
                .contains(expected_agent));
            assert!(options
                .entrypoint
                .as_ref()
                .unwrap()
                .0
                .iter()
                .any(|part| part == expected_agent));
            assert!(
                options.overlays.iter().any(|o| o.host_path == build_repo),
                "repair must mount the parent checkout"
            );
            assert!(
                prompt.contains("{{work_item_content}}"),
                "diagnostic tokens must remain literal"
            );
            assert!(!prompt.contains("OUTSIDE_SENTINEL"));
            assert!(
                prompt.contains("````text"),
                "diagnostic fences must outlast injected delimiters"
            );
            if matches!(case, RepairCase::ImageFailure) {
                return Err(EngineError::ContainerImageNotFound {
                    image: "repair-image".into(),
                });
            }
            if matches!(case, RepairCase::BuildFailure) {
                return Err(EngineError::Other("injected repair build failure".into()));
            }
            if !matches!(case, RepairCase::Unresolved | RepairCase::StartFailure) {
                std::fs::write(
                    build_repo.join("conflict-{{work_item_content}}-```.txt"),
                    "both\n",
                )
                .unwrap();
            }
            Ok(())
        }),
        Some(Arc::new(move || {
            if matches!(case, RepairCase::StartFailure)
                && starting_repair.load(std::sync::atomic::Ordering::SeqCst)
            {
                Err(EngineError::Other("injected repair start failure".into()))
            } else {
                Ok(())
            }
        })),
    );
    // The conflicting filename injects a template token and fence delimiter
    // into Git's diagnostics without executing repository code on the host.
    let mut engines = Engines::for_tests(&repo);
    engines.runtime = runtime.clone();
    engines.container_runtime = Some(runtime.clone());
    engines.agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        engines.overlay_engine.clone(),
        runtime,
    ));
    let session = Session::for_tests_isolated(&repo, &tmp.path().join("config-home"));
    let mut flags = flags(None);
    flags.on_merge_failure = Some(OnMergeFailure::Agent);
    flags.max_concurrent = Some(2);
    if matches!(case, RepairCase::OverrideIdentity) {
        flags.agent = Some("codex".into());
        flags.model = Some("override".into());
    }
    let result = ExecWorkflowCommand::new(flags, engines, session)
        .run_with_frontend(Box::new(frontend))
        .await;
    let calls = calls.lock().unwrap();
    if matches!(case, RepairCase::RejectedTemplate) {
        assert!(
            calls.is_empty(),
            "unsafe template must not reach the runtime"
        );
    } else {
        assert_eq!(calls.len(), 1, "exactly one repair launch");
        let expected = if matches!(case, RepairCase::OverrideIdentity) {
            "override"
        } else {
            "beta"
        };
        assert_eq!(calls[0].1, Some(ModelFlagForm::Argument(expected.into())));
    }
    let payloads: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .map(|e| e.payload)
        .collect();
    match case {
        RepairCase::Resolve | RepairCase::OverrideIdentity => {
            result.unwrap();
            assert!(!worktree.exists());
            assert_eq!(
                std::fs::read_to_string(repo.join("conflict-{{work_item_content}}-```.txt"))
                    .unwrap(),
                "both\n"
            );
            assert!(repo.join("feature-only.txt").exists());
            // Leader + two main steps + repair. Each writes through the
            // ordinary API agent frontend, including the final repair.
            assert_eq!(payloads.iter().filter(|p| matches!(p, EventPayload::StatusMessage { message, .. } if message.contains("held runtime message"))).count(), 4);
        }
        RepairCase::Unresolved => {
            assert!(matches!(result, Err(CommandError::MergeConflict { .. })));
            assert!(worktree.exists());
        }
        RepairCase::BuildFailure
        | RepairCase::ImageFailure
        | RepairCase::StartFailure
        | RepairCase::RejectedTemplate => {
            result.unwrap(); // current keep policy reports the failure and preserves work
            assert!(worktree.exists());
            assert!(payloads
                .iter()
                .any(|p| matches!(p, EventPayload::StatusMessage { message, .. }
                if message.contains("merge-fix:") || message.contains("did not run"))));
        }
    }
    // Held frontends can have output tasks; release them before fixture cleanup.
    held.lock().unwrap().clear();
    if worktree.exists() {
        let _ = std::fs::remove_dir_all(worktree);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_merge_fix_launches_once_with_parallel_identity_and_streams_output() {
    run_merge_repair(RepairCase::Resolve).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_merge_fix_respects_explicit_identity_overrides() {
    run_merge_repair(RepairCase::OverrideIdentity).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_merge_fix_runtime_build_failure_keeps_worktree() {
    run_merge_repair(RepairCase::BuildFailure).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_merge_fix_unresolved_conflict_keeps_worktree() {
    run_merge_repair(RepairCase::Unresolved).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_merge_fix_rejects_host_template_before_runtime_launch() {
    run_merge_repair(RepairCase::RejectedTemplate).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_initial_dirty_checkout_is_committed_before_final_merge_fail_policy() {
    let (run, _tmp) =
        run_api_dynamic_workflow_with_initial_dirty(Some(OnDirtyMergeTarget::Fail), false, true)
            .await;
    run.result.unwrap();
    assert_eq!(
        git(&run.repo, &["show", "HEAD:initial-draft.txt"]),
        "initial user work"
    );
    assert!(git(&run.repo, &["status", "--porcelain"]).is_empty());
    assert!(!run.worktree.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_merge_fix_runtime_start_failure_keeps_worktree() {
    run_merge_repair(RepairCase::StartFailure).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_merge_fix_missing_image_keeps_worktree() {
    run_merge_repair(RepairCase::ImageFailure).await;
}
