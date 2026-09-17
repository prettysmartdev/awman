//! Part 1 task schema, due-selection, and daemon-gateway tests.

use std::sync::{Arc, Mutex};

use awman::command::commands::squad::gateway::{
    CreateTask, LocalTaskGateway, TaskGateway, UpdateTask,
};
use awman::command::dispatch::Engines;
use awman::data::fs::{
    AuthPathResolver, DataPaths, MountScope, RunDetail, RunStatus, Task, TaskStatus, TaskStore,
    TaskWorkspace,
};
use awman::data::EngineWorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::engine::squad::SchedulerStatus;
use chrono::{Duration, Utc};

fn task(name: &str, now: chrono::DateTime<Utc>) -> Task {
    Task {
        id: format!("id-{name}"),
        name: name.into(),
        description: format!("when {name} happens"),
        repo_scope: "/repo".into(),
        mount_scope: MountScope::GitRoot,
        overlays: Vec::new(),
        interval_secs: 300,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now - Duration::hours(1),
        updated_at: now - Duration::hours(1),
        last_run_at: None,
        trigger_requested_at: None,
        last_run_status: None,
        unmet_env: Vec::new(),
    }
}

fn test_engines(root: &std::path::Path) -> Engines {
    let api_paths = awman::data::fs::ApiPaths::from_root(root.join("api"));
    let auth_paths = AuthPathResolver::at_home(root);
    let runtime = Arc::new(ContainerRuntime::docker());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth_paths.clone()));
    let agent = Arc::new(AgentEngine::new(overlay.clone(), runtime.clone()));
    Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::new()),
        overlay_engine: overlay,
        auth_engine: Arc::new(AuthEngine::with_paths(auth_paths, api_paths)),
        agent_engine: agent,
        workflow_state_store: Arc::new(EngineWorkflowStateStore::at_git_root(root)),
    }
}

#[test]
fn task_store_schema_migration_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let first = TaskStore::open(&db).unwrap();
    first.migrate().unwrap();
    drop(first);
    let second = TaskStore::open(&db).unwrap();
    second.migrate().unwrap();
    assert!(second.list().unwrap().is_empty());
    drop(second);
    // A third open + migrate verifies that both CREATE TABLE IF NOT EXISTS and
    // the additive-column migration remain no-ops after repeated startup, and
    // that `migrate` is callable more than once against one open store.
    let third = TaskStore::open(&db).unwrap();
    third.migrate().unwrap();
    third.migrate().unwrap();
    assert!(third.list().unwrap().is_empty());
}

#[test]
fn squad_schema_and_workspace_overlay_fields_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = Utc::now();
    let workspace = tmp.path().join("squad/tasks/durable/workspace");
    let stored = Task {
        id: "durable-id".into(),
        name: "durable".into(),
        description: "preserve task state".into(),
        // `TaskWorkspace` is resolved before persistence.  This is the
        // persisted representation of the Default Task Workspace choice.
        repo_scope: workspace.clone(),
        mount_scope: MountScope::Directory,
        overlays: vec![
            "dir(/host/data:/task-data:ro)".into(),
            "env(SQUAD_TOKEN)".into(),
            "skill(review)".into(),
        ],
        interval_secs: 6 * 60 * 60,
        status: TaskStatus::Active,
        agent: Some("codex".into()),
        model: Some("gpt-5".into()),
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
        trigger_requested_at: None,
        last_run_status: None,
        unmet_env: Vec::new(),
    };
    store.create(&stored).unwrap();

    assert_eq!(store.get("durable").unwrap(), Some(stored.clone()));
    assert!(
        !stored.uses_worktree(),
        "the persisted default-workspace representation must mount directly"
    );

    let run_id = store.start_run(&stored.id, Some("session"), now).unwrap();
    store
        .finish_run(&run_id, RunStatus::NotTriggered, &RunDetail::default(), now)
        .unwrap();
    let runs = store.runs_for("durable", 1).unwrap();
    assert_eq!(runs[0].task_id, stored.id);
    assert_eq!(runs[0].status, RunStatus::NotTriggered);

    let conn = rusqlite::Connection::open(db).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(tables.contains(&"squad_tasks".to_string()));
    assert!(tables.contains(&"squad_runs".to_string()));
    assert!(
        !tables
            .iter()
            .any(|name| name.contains("amie") || name.contains("conditions")),
        "the unreleased rename must not create legacy schema tables: {tables:?}"
    );
}

#[tokio::test]
async fn malformed_task_overlay_is_rejected_before_store_or_workspace_write() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = Arc::new(TaskStore::open(&db).unwrap());
    store.migrate().unwrap();
    let paths = awman::data::fs::SquadPaths::from_root(tmp.path().join("squad"));
    let gateway = LocalTaskGateway::new(
        store.clone(),
        test_engines(tmp.path()),
        Arc::new(Mutex::new(SchedulerStatus::default())),
        paths.clone(),
        Arc::new(awman::engine::squad::env_state::DaemonEnvState::without_store()),
    );
    let request = CreateTask {
        name: "bad-overlay".into(),
        description: "must fail before persistence".into(),
        workspace: TaskWorkspace::Default,
        mount_scope: MountScope::Directory,
        interval_secs: 6 * 60 * 60,
        agent: None,
        model: None,
        overlays: vec!["not-an-overlay".into()],
        agents_to_models: Default::default(),
    };

    let error = gateway.create(request).await.unwrap_err();
    assert!(error.to_string().contains("overlay"), "{error}");
    assert!(
        store.list().unwrap().is_empty(),
        "no invalid task may be stored"
    );
    assert!(
        !paths.task_dir("bad-overlay").unwrap().exists(),
        "validation failure must not create a durable workspace"
    );
}

#[test]
fn due_for_evaluation_applies_pause_backoff_interval_and_running_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = Utc::now();

    let due_fresh = task("fresh", now);
    let due_elapsed = Task {
        last_run_at: Some(now - Duration::seconds(301)),
        last_run_status: None,
        unmet_env: Vec::new(),
        ..task("elapsed", now)
    };
    let paused = Task {
        status: TaskStatus::Paused,
        ..task("paused", now)
    };
    let backed_off = Task {
        backoff_until: Some(now + Duration::minutes(5)),
        ..task("backoff", now)
    };
    let interval_not_elapsed = Task {
        interval_secs: 3600,
        last_run_at: Some(now - Duration::seconds(10)),
        last_run_status: None,
        unmet_env: Vec::new(),
        ..task("interval", now)
    };
    let running = task("running", now);

    for item in [
        due_fresh.clone(),
        due_elapsed.clone(),
        paused,
        backed_off,
        interval_not_elapsed,
        running.clone(),
    ] {
        store.create(&item).unwrap();
    }
    store.start_run(&running.id, Some("session"), now).unwrap();

    let names: std::collections::BTreeSet<_> = store
        .due_for_evaluation(now)
        .unwrap()
        .into_iter()
        .map(|item| item.name)
        .collect();
    assert_eq!(
        names,
        ["fresh", "elapsed"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
}

/// `squad trigger` overrides the two rules that are about *when* a task is
/// due — the interval and the backoff — and neither of the two that are about
/// whether it may run at all.
#[test]
fn a_trigger_request_overrides_the_interval_and_backoff_but_not_pause_or_a_running_run() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = Utc::now();

    let not_elapsed = Task {
        interval_secs: 3600,
        last_run_at: Some(now - Duration::seconds(10)),
        ..task("not-elapsed", now)
    };
    let backed_off = Task {
        interval_secs: 3600,
        last_run_at: Some(now - Duration::seconds(10)),
        backoff_until: Some(now + Duration::minutes(30)),
        ..task("backed-off", now)
    };
    let paused = Task {
        interval_secs: 3600,
        last_run_at: Some(now - Duration::seconds(10)),
        status: TaskStatus::Paused,
        ..task("paused", now)
    };
    let running = Task {
        interval_secs: 3600,
        last_run_at: Some(now - Duration::seconds(10)),
        ..task("running", now)
    };
    for item in [
        not_elapsed.clone(),
        backed_off.clone(),
        paused.clone(),
        running.clone(),
    ] {
        store.create(&item).unwrap();
    }
    store.start_run(&running.id, None, now).unwrap();

    assert!(
        store.due_for_evaluation(now).unwrap().is_empty(),
        "none of these tasks is due on its own schedule"
    );

    for name in ["not-elapsed", "backed-off", "paused", "running"] {
        assert!(store.request_trigger(name, now).unwrap(), "{name} exists");
    }
    assert!(
        !store.request_trigger("no-such-task", now).unwrap(),
        "triggering a task that does not exist reports so rather than succeeding"
    );

    let due: std::collections::BTreeSet<_> = store
        .due_for_evaluation(now)
        .unwrap()
        .into_iter()
        .map(|item| item.name)
        .collect();
    assert_eq!(
        due,
        ["not-elapsed", "backed-off"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        "a trigger beats the interval and the backoff; it does not un-pause a \
         task or start a second concurrent run"
    );

    // The trigger is recorded on the task so a frontend can show that it is
    // pending, and cleared the moment the run it asked for opens — so the next
    // tick selects the task on its schedule again, not a second time.
    let task = store.get("not-elapsed").unwrap().unwrap();
    assert!(task.trigger_requested_at.is_some());
    assert_eq!(
        task.backoff_until, None,
        "an explicit trigger clears the backoff it overrides"
    );

    store.start_run(&not_elapsed.id, None, now).unwrap();
    let task = store.get("not-elapsed").unwrap().unwrap();
    assert_eq!(
        task.trigger_requested_at, None,
        "opening the run honours the trigger and consumes it"
    );
    assert!(
        store
            .due_for_evaluation(now + Duration::seconds(1))
            .unwrap()
            .iter()
            .all(|item| item.name != "not-elapsed"),
        "a consumed trigger must not re-fire on the following tick"
    );
}

#[tokio::test]
async fn task_store_crud_is_exercised_through_daemon_gateway() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = Arc::new(TaskStore::open(&db).unwrap());
    store.migrate().unwrap();
    let gateway = LocalTaskGateway::new(
        store,
        test_engines(tmp.path()),
        Arc::new(Mutex::new(SchedulerStatus::default())),
        awman::data::fs::SquadPaths::from_root(tmp.path().join("squad")),
        Arc::new(awman::engine::squad::env_state::DaemonEnvState::without_store()),
    );
    let request = || CreateTask {
        name: "issue-triage".into(),
        description: "when an issue is opened".into(),
        workspace: TaskWorkspace::Custom(repo.clone()),
        mount_scope: MountScope::GitRoot,
        interval_secs: 300,
        agent: None,
        model: None,
        overlays: Vec::new(),
        agents_to_models: Default::default(),
    };

    let created = gateway.create(request()).await.unwrap();
    assert_eq!(created.name, "issue-triage");
    assert_eq!(gateway.list().await.unwrap().len(), 1);
    assert_eq!(gateway.get("issue-triage").await.unwrap(), created);

    let duplicate = gateway
        .create(request())
        .await
        .expect_err("duplicate names must be rejected by the gateway");
    assert!(
        duplicate.to_string().contains("issue-triage"),
        "unique-name error should identify the existing name: {duplicate}"
    );

    gateway
        .set_status("issue-triage", TaskStatus::Paused)
        .await
        .unwrap();
    assert_eq!(
        gateway.get("issue-triage").await.unwrap().status,
        TaskStatus::Paused
    );
    gateway
        .set_status("issue-triage", TaskStatus::Active)
        .await
        .unwrap();
    gateway.delete("issue-triage").await.unwrap();
    assert!(gateway.list().await.unwrap().is_empty());
    assert!(gateway.get("issue-triage").await.is_err());
}

/// WI 0106 Part 5: the task grid shows every card's last-run *outcome*, so the
/// store reads it alongside the task in one statement rather than making the
/// UI ask for run history per card.
#[test]
fn list_and_get_carry_each_tasks_latest_run_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = Utc::now();

    let never_run = task("never-run", now);
    let ran = task("ran", now);
    store.create(&never_run).unwrap();
    store.create(&ran).unwrap();

    assert_eq!(store.get("ran").unwrap().unwrap().last_run_status, None);

    // Two runs: the older triggered a workflow, the newer did not. Only the
    // newer one is the "last run".
    let first = store
        .start_run(&ran.id, None, now - Duration::hours(2))
        .unwrap();
    store
        .finish_run(
            &first,
            RunStatus::WorkflowExecuted,
            &RunDetail::default(),
            now - Duration::hours(2),
        )
        .unwrap();
    let second = store
        .start_run(&ran.id, None, now - Duration::minutes(5))
        .unwrap();
    store
        .finish_run(
            &second,
            RunStatus::NotTriggered,
            &RunDetail::default(),
            now - Duration::minutes(5),
        )
        .unwrap();

    assert_eq!(
        store.get("ran").unwrap().unwrap().last_run_status,
        Some(RunStatus::NotTriggered),
        "get must report the most recent run's outcome"
    );

    let listed = store.list().unwrap();
    let by_name = |name: &str| {
        listed
            .iter()
            .find(|task| task.name == name)
            .unwrap()
            .last_run_status
    };
    assert_eq!(by_name("ran"), Some(RunStatus::NotTriggered));
    assert_eq!(
        by_name("never-run"),
        None,
        "a task that has never run reports no outcome, not a neighbour's"
    );
}

/// WI 0106 §2b: worktree isolation is derived from "the effective task root
/// **is** a git repository root", not from "the effective task root is
/// somewhere inside one".
///
/// A worktree is a checkout of the whole repository, so worktree-isolating a
/// task bound to a *subdirectory* would silently hand the run the entire
/// enclosing repository instead of the folder the user picked — a widening the
/// captured mount scope exists to prevent. A subdirectory is exactly the "not
/// a git root" case the interview warns about, so it is bound as the plain
/// directory it is: direct mount, no worktree.
#[tokio::test]
async fn a_custom_workspace_below_a_repository_root_is_a_plain_directory_and_never_worktree_isolated(
) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_test_repo(&repo);
    let nested = repo.join("services").join("api");
    std::fs::create_dir_all(&nested).unwrap();

    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = Arc::new(TaskStore::open(&db).unwrap());
    store.migrate().unwrap();
    let gateway = LocalTaskGateway::new(
        store,
        test_engines(tmp.path()),
        Arc::new(Mutex::new(SchedulerStatus::default())),
        awman::data::fs::SquadPaths::from_root(tmp.path().join("squad")),
        Arc::new(awman::engine::squad::env_state::DaemonEnvState::without_store()),
    );

    // The repository root itself keeps its captured scope and is
    // worktree-isolated: a worktree of the root mounts exactly the root.
    let at_root = gateway
        .create(CreateTask {
            name: "at-root".into(),
            description: "bound to the repository root".into(),
            workspace: TaskWorkspace::Custom(repo.clone()),
            mount_scope: MountScope::GitRoot,
            interval_secs: 6 * 60 * 60,
            agent: None,
            model: None,
            overlays: Vec::new(),
            agents_to_models: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(at_root.mount_scope, MountScope::GitRoot);
    assert!(
        at_root.uses_worktree(),
        "a task bound to a repository root is always worktree-isolated"
    );

    // A subdirectory of that same repository is not. Even asked for with
    // `--mount-scope gitroot`, it is stored as the directory it is.
    let below_root = gateway
        .create(CreateTask {
            name: "below-root".into(),
            description: "bound to a subdirectory".into(),
            workspace: TaskWorkspace::Custom(nested.clone()),
            mount_scope: MountScope::GitRoot,
            interval_secs: 6 * 60 * 60,
            agent: None,
            model: None,
            overlays: Vec::new(),
            agents_to_models: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        below_root.mount_scope,
        MountScope::Directory,
        "a path below the repository root is bound as a plain directory"
    );
    assert!(
        !below_root.uses_worktree(),
        "worktree isolation would widen this run's view to the whole repository"
    );
    assert_eq!(
        below_root.repo_scope,
        nested.canonicalize().unwrap(),
        "the effective root stays the folder the user chose"
    );
}

fn init_test_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .expect("git must run");
    assert!(status.success(), "git init must succeed");
}

/// WI 0110 regression: `squad_runs.task_id` references `squad_tasks(id)`, so a
/// delete that only touched `squad_tasks` failed with `FOREIGN KEY constraint
/// failed` the moment a task had been evaluated even once — which is why
/// deleting a task from the TUI appeared to do nothing. The store removes a
/// task's runs with the task, and leaves every other task's runs alone.
#[test]
fn deleting_a_task_that_has_run_history_removes_the_task_and_its_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = TaskStore::open(&db).unwrap();
    store.migrate().unwrap();
    let now = Utc::now();

    let doomed = task("doomed", now);
    let keeper = task("keeper", now);
    store.create(&doomed).unwrap();
    store.create(&keeper).unwrap();

    // A finished run and a still-open one: both reference the task row.
    let finished = store.start_run(&doomed.id, None, now).unwrap();
    store
        .finish_run(&finished, RunStatus::Failed, &RunDetail::default(), now)
        .unwrap();
    store.start_run(&doomed.id, None, now).unwrap();
    store.start_run(&keeper.id, None, now).unwrap();

    assert!(
        store.delete("doomed").unwrap(),
        "deleting a task with run history must succeed, not fail the FK check"
    );
    assert!(store.get("doomed").unwrap().is_none());
    assert!(store.runs_for("doomed", 10).unwrap().is_empty());

    assert!(store.get("keeper").unwrap().is_some());
    assert_eq!(
        store.runs_for("keeper", 10).unwrap().len(),
        1,
        "another task's run history must be untouched"
    );

    assert!(
        !store.delete("doomed").unwrap(),
        "a second delete reports that there was nothing to remove"
    );
}

// ─── WI 0110: editing a task, and its own config.json ───────────────────────

/// Build a gateway plus its squad paths, sharing one temp root.
fn edit_fixture(
    tmp: &tempfile::TempDir,
) -> (
    Arc<TaskStore>,
    LocalTaskGateway,
    awman::data::fs::SquadPaths,
) {
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = Arc::new(TaskStore::open(&db).unwrap());
    store.migrate().unwrap();
    let paths = awman::data::fs::SquadPaths::from_root(tmp.path().join("squad"));
    let gateway = LocalTaskGateway::new(
        store.clone(),
        test_engines(tmp.path()),
        Arc::new(Mutex::new(SchedulerStatus::default())),
        paths.clone(),
        Arc::new(awman::engine::squad::env_state::DaemonEnvState::without_store()),
    );
    (store, gateway, paths)
}

fn editable_task(name: &str) -> CreateTask {
    CreateTask {
        name: name.into(),
        description: "when something happens, do something".into(),
        workspace: TaskWorkspace::Default,
        mount_scope: MountScope::Directory,
        interval_secs: 6 * 60 * 60,
        agent: Some("claude".into()),
        model: Some("claude-opus-4-8".into()),
        overlays: vec!["env(GITHUB_TOKEN)".into()],
        agents_to_models: Default::default(),
    }
}

/// An edit changes only the fields it carries, and leaves the capture-once
/// workspace identity alone — there is no way to express a change to it.
#[tokio::test]
async fn an_edit_changes_only_the_fields_it_carries() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, gateway, _paths) = edit_fixture(&tmp);
    let created = gateway.create(editable_task("editable")).await.unwrap();

    let updated = gateway
        .update(
            "editable",
            UpdateTask {
                description: Some("a new description".into()),
                interval_secs: Some(600),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.description, "a new description");
    assert_eq!(updated.interval_secs, 600);
    assert_eq!(
        updated.agent.as_deref(),
        Some("claude"),
        "a field the edit did not mention keeps its value"
    );
    assert_eq!(updated.overlays, created.overlays);
    assert_eq!(updated.repo_scope, created.repo_scope);
    assert_eq!(updated.mount_scope, created.mount_scope);
    assert_eq!(updated.created_at, created.created_at);
    assert!(updated.updated_at >= created.updated_at);
}

/// `Some(None)` is the "clear this back to the squad default" answer, and must
/// be distinguishable from "not mentioned".
#[tokio::test]
async fn clearing_the_agent_and_model_is_distinct_from_leaving_them_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, gateway, _paths) = edit_fixture(&tmp);
    gateway.create(editable_task("clearable")).await.unwrap();

    let untouched = gateway
        .update(
            "clearable",
            UpdateTask {
                description: Some("still has an agent".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(untouched.agent.as_deref(), Some("claude"));

    let cleared = gateway
        .update(
            "clearable",
            UpdateTask {
                agent: Some(None),
                model: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(cleared.agent, None);
    assert_eq!(cleared.model, None);
}

/// Overlays are replace-or-keep: a `Some` list replaces the stored one whole,
/// and an empty `Some` clears it. The rules creation enforces still apply.
#[tokio::test]
async fn overlays_are_replaced_wholesale_and_still_validated() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, gateway, _paths) = edit_fixture(&tmp);
    gateway.create(editable_task("overlaid")).await.unwrap();

    let replaced = gateway
        .update(
            "overlaid",
            UpdateTask {
                overlays: Some(vec!["env(A)".into(), "env(B)".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        replaced.overlays,
        vec!["env(A)".to_string(), "env(B)".to_string()]
    );

    let cleared = gateway
        .update(
            "overlaid",
            UpdateTask {
                overlays: Some(Vec::new()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(cleared.overlays.is_empty());

    let error = gateway
        .update(
            "overlaid",
            UpdateTask {
                overlays: Some(vec!["not-an-overlay".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("overlay"), "{error}");
}

/// An edit must not be able to install an interval `squad add` would refuse.
#[tokio::test]
async fn an_edited_interval_is_held_to_the_creation_bounds() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, gateway, _paths) = edit_fixture(&tmp);
    gateway.create(editable_task("bounded")).await.unwrap();

    for interval in [30u64, 200_000] {
        let error = gateway
            .update(
                "bounded",
                UpdateTask {
                    interval_secs: Some(interval),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("task interval must be between"),
            "{interval}s should be refused: {error}"
        );
    }
}

#[tokio::test]
async fn editing_an_unknown_task_reports_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, gateway, _paths) = edit_fixture(&tmp);
    let error = gateway
        .update(
            "never-created",
            UpdateTask {
                description: Some("x".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("was not found"), "{error}");
}

/// The task's agent pool is written to its own `config.json`, in the same
/// document shape as the global config, and an empty pool removes the file
/// rather than writing an empty block.
#[tokio::test]
async fn the_task_agent_pool_round_trips_through_its_own_config_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, gateway, paths) = edit_fixture(&tmp);

    let mut pool = std::collections::BTreeMap::new();
    pool.insert(
        "claude".to_string(),
        vec![
            "claude-opus-4-8".to_string(),
            "claude-sonnet-4-6".to_string(),
        ],
    );
    gateway
        .create(CreateTask {
            agents_to_models: pool.clone(),
            ..editable_task("pooled")
        })
        .await
        .unwrap();

    let config_path = paths.task_config_file("pooled").unwrap();
    let document = awman::data::config::global::GlobalConfig::load_path(&config_path).unwrap();
    let squad = document
        .squad
        .expect("the task config carries a squad block");
    assert_eq!(
        squad
            .agents_to_models
            .as_ref()
            .unwrap()
            .get("claude")
            .unwrap(),
        &vec![
            "claude-opus-4-8".to_string(),
            "claude-sonnet-4-6".to_string()
        ]
    );

    // Emptying the pool removes the file, so the task goes back to inheriting
    // the global block cleanly rather than carrying an empty override.
    gateway
        .update(
            "pooled",
            UpdateTask {
                agents_to_models: Some(Default::default()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !config_path.exists(),
        "an emptied pool removes the task config instead of writing an empty block"
    );
}

/// The scheduler's per-tick read: the task block wins field-by-field over the
/// global one, and a task with no file inherits the global block whole.
#[tokio::test]
async fn a_task_config_layers_over_the_global_squad_block() {
    use awman::data::config::global::task_squad_config;
    use awman::data::config::repo::SquadConfig;

    let tmp = tempfile::tempdir().unwrap();
    let (_store, gateway, paths) = edit_fixture(&tmp);
    let global = SquadConfig {
        agents_to_models: Some(std::collections::HashMap::from([(
            "claude".to_string(),
            vec!["claude-opus-4-8".to_string()],
        )])),
        default_leader: Some("claude::claude-opus-4-8".to_string()),
        guidance: Some(vec!["Keep changes focused.".to_string()]),
        max_concurrent_evaluations: Some(3),
        env_persistence: None,
    };

    gateway.create(editable_task("inheritor")).await.unwrap();
    assert_eq!(
        task_squad_config(&paths, "inheritor", &global).unwrap(),
        global,
        "a task with no config.json inherits the global block whole"
    );

    let mut pool = std::collections::BTreeMap::new();
    pool.insert("codex".to_string(), vec!["gpt-5".to_string()]);
    gateway
        .create(CreateTask {
            agents_to_models: pool,
            ..editable_task("overrider")
        })
        .await
        .unwrap();

    let effective = task_squad_config(&paths, "overrider", &global).unwrap();
    assert_eq!(
        effective
            .agents_to_models
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["codex"],
        "the task's pool replaces the global pool for that task only"
    );
    assert_eq!(
        effective.guidance, global.guidance,
        "guidance the task did not override is still applied"
    );
}

/// A hand-edited task file that does not parse is an error the run reports,
/// not a silent fall back to the global pool.
#[test]
fn a_malformed_task_config_is_reported_rather_than_ignored() {
    use awman::data::config::global::task_squad_config;
    use awman::data::config::repo::SquadConfig;

    let tmp = tempfile::tempdir().unwrap();
    let paths = awman::data::fs::SquadPaths::from_root(tmp.path().join("squad"));
    let config_path = paths.task_config_file("broken").unwrap();
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, "{not json").unwrap();

    let error = task_squad_config(&paths, "broken", &SquadConfig::default()).unwrap_err();
    assert!(
        error.to_string().contains("config.json"),
        "the error should name the file that is broken: {error}"
    );
}

// ─── squad trigger ──────────────────────────────────────────────────────────

/// The gateway is where "triggering a paused task" is answered, because it is
/// the only layer that can say *why*: the store would happily record the
/// request, and `due_for_evaluation` would then quietly never select it, so
/// the user would be left watching a task they triggered do nothing.
#[tokio::test]
async fn triggering_refuses_a_paused_or_unknown_task_and_arms_an_active_one() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, gateway, _paths) = edit_fixture(&tmp);
    gateway.create(editable_task("triggerable")).await.unwrap();

    gateway
        .trigger("triggerable")
        .await
        .expect("an active task can be triggered");
    assert!(
        store
            .get("triggerable")
            .unwrap()
            .unwrap()
            .trigger_requested_at
            .is_some(),
        "the request is recorded on the task"
    );

    let missing = gateway.trigger("no-such-task").await.unwrap_err();
    assert!(
        missing.to_string().contains("was not found"),
        "an unknown task is named as such: {missing}"
    );

    gateway
        .set_status("triggerable", TaskStatus::Paused)
        .await
        .unwrap();
    let paused = gateway.trigger("triggerable").await.unwrap_err();
    let message = paused.to_string();
    assert!(
        message.contains("paused") && message.contains("squad resume triggerable"),
        "refusing a paused task must say what to do about it: {message}"
    );
}

/// A trigger changes nothing about the task's configuration — that is the
/// whole point of having it rather than telling users to shorten the interval.
#[tokio::test]
async fn triggering_leaves_the_tasks_schedule_and_every_other_field_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, gateway, _paths) = edit_fixture(&tmp);
    let before = gateway.create(editable_task("untouched")).await.unwrap();

    gateway.trigger("untouched").await.unwrap();
    let after = gateway.get("untouched").await.unwrap();

    assert_eq!(after.interval_secs, before.interval_secs);
    assert_eq!(after.status, before.status);
    assert_eq!(after.agent, before.agent);
    assert_eq!(after.model, before.model);
    assert_eq!(after.overlays, before.overlays);
    assert_eq!(
        after.last_run_at, before.last_run_at,
        "the last-run timestamp still says when the task last actually ran"
    );
}

/// `squad cancel` against a live scheduler: the in-flight evaluation is
/// abandoned, the run is recorded `canceled` with no backoff, and a second
/// cancel — with nothing left running — is refused.
#[tokio::test]
async fn cancel_stops_an_in_flight_run_and_records_it_as_canceled() {
    use awman::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use awman::engine::squad::{
        EvaluationOutcome, EvaluationRequest, SquadScheduler, TaskEvaluator,
    };

    struct NeverFinishes;

    #[async_trait::async_trait]
    impl TaskEvaluator for NeverFinishes {
        async fn evaluate(&self, _request: EvaluationRequest) -> EvaluationOutcome {
            std::future::pending::<()>().await;
            unreachable!("a pending evaluation never completes")
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let db = DataPaths::at_root(tmp.path().join("data")).db_path();
    let store = Arc::new(TaskStore::open(&db).unwrap());
    store.migrate().unwrap();
    let paths = awman::data::fs::SquadPaths::from_root(tmp.path().join("squad"));
    let env = EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, tmp.path().to_str().unwrap())]);
    store.create(&task("long-run", Utc::now())).unwrap();

    let scheduler = SquadScheduler::new(store.clone(), paths.clone(), Arc::new(NeverFinishes), env)
        .with_tick_interval(std::time::Duration::from_millis(50));
    let status = scheduler.status_handle();
    let gateway = LocalTaskGateway::new(
        store.clone(),
        test_engines(tmp.path()),
        status.clone(),
        paths,
        Arc::new(awman::engine::squad::env_state::DaemonEnvState::without_store()),
    );

    let error = gateway.cancel("long-run").await.unwrap_err();
    assert!(error.to_string().contains("no run in progress"), "{error}");

    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = tokio::spawn(scheduler.run(shutdown.clone()));

    let runs_with = |wanted: RunStatus| {
        let store = store.clone();
        async move {
            for _ in 0..100 {
                let runs = store.runs_for("long-run", 10).unwrap();
                if runs.first().is_some_and(|run| run.status == wanted) {
                    return runs;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("the run never reached {wanted:?}");
        }
    };
    runs_with(RunStatus::Running).await;

    gateway.cancel("long-run").await.unwrap();
    let runs = runs_with(RunStatus::Canceled).await;
    assert_eq!(runs.len(), 1, "cancel must not start another run");
    assert!(runs[0].finished_at.is_some());
    assert!(runs[0].error.is_none());

    // The evaluation's bookkeeping is released and no backoff was set.
    for _ in 0..100 {
        if status.lock().unwrap().in_flight == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(status.lock().unwrap().in_flight, 0);
    assert!(status.lock().unwrap().cancellations.is_empty());
    assert!(store
        .get("long-run")
        .unwrap()
        .unwrap()
        .backoff_until
        .is_none());

    let error = gateway.cancel("long-run").await.unwrap_err();
    assert!(error.to_string().contains("no run in progress"), "{error}");

    shutdown.cancel();
    handle.await.unwrap();
}
