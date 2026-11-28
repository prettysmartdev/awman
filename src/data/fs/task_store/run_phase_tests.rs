//! The run phase's persistence contract (WI 0105), against a private
//! in-memory database: backward compatibility, transitions, and the final
//! outcomes that must clear it.

use super::*;
use chrono::TimeZone;
use serde_json::json;

fn empty_store() -> TaskStore {
    TaskStore::from_connection(std::sync::Arc::new(Mutex::new(
        Connection::open_in_memory().unwrap(),
    )))
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn task(name: &str) -> Task {
    Task {
        prerequisites: None,
        id: TaskId::new(),
        name: name.into(),
        description: "d".into(),
        repo_scope: PathBuf::from("workspace"),
        mount_scope: MountScope::Directory,
        overlays: vec![],
        interval_secs: 60,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now(),
        updated_at: now(),
        last_run_at: None,
        trigger_requested_at: None,
        last_run_status: None,
        last_run_phase: None,
        unmet_env: vec![],
    }
}

fn running() -> (TaskStore, Task, RunId) {
    let store = empty_store();
    store.migrate().unwrap();
    let task = task("alpha");
    store.create(&task).unwrap();
    let run = store.start_run(&task.id, None, now()).unwrap();
    (store, task, run)
}

fn only_run(store: &TaskStore) -> Run {
    let mut runs = store.runs_for("alpha", 10).unwrap();
    assert_eq!(runs.len(), 1);
    runs.remove(0)
}

/// The task's derived phase, which every task query must agree on.
fn task_phase(store: &TaskStore) -> Option<RunPhase> {
    let one = store.get("alpha").unwrap().unwrap();
    assert_eq!(store.list().unwrap()[0].last_run_phase, one.last_run_phase);
    one.last_run_phase
}

#[test]
fn the_phase_is_additive_on_the_wire() {
    assert_eq!(
        serde_json::to_value(RunPhase::Building).unwrap(),
        "building"
    );
    let (store, _, run) = running();

    // Without a phase the records serialize without the new keys, and records
    // written without them read back.
    let idle = serde_json::to_value(only_run(&store)).unwrap();
    assert!(idle.get("phase").is_none() && idle.get("image_warning").is_none());
    let idle: Run = serde_json::from_value(idle).unwrap();
    assert_eq!((idle.phase, idle.image_warning), (None, None));
    let task = serde_json::to_value(store.get("alpha").unwrap().unwrap()).unwrap();
    assert!(task.get("last_run_phase").is_none());
    let task: Task = serde_json::from_value(task).unwrap();
    assert_eq!(task.last_run_phase, None);

    store.set_run_phase(&run, Some(RunPhase::Building)).unwrap();
    let building = serde_json::to_value(only_run(&store)).unwrap();
    assert_eq!(building["status"], json!("running"));
    assert_eq!(building["phase"], json!("building"));
    let building: Run = serde_json::from_value(building).unwrap();
    assert_eq!(building.phase, Some(RunPhase::Building));
    let task = serde_json::to_value(store.get("alpha").unwrap().unwrap()).unwrap();
    assert_eq!(task["last_run_status"], json!("running"));
    assert_eq!(task["last_run_phase"], json!("building"));
}

#[test]
fn an_older_database_gains_the_columns_and_its_rows_have_no_phase() {
    let store = empty_store();
    let task_id = TaskId::new();
    let run_id = RunId::new();
    {
        // The schema as first released, before any added column.
        let conn = store.lock();
        conn.execute_batch(
            "CREATE TABLE squad_tasks (
                id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, description TEXT NOT NULL,
                repo_scope TEXT NOT NULL, mount_scope TEXT NOT NULL, overlays TEXT,
                interval_secs INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'active',
                agent TEXT, model TEXT, backoff_until TEXT, created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL, last_run_at TEXT
             );
             CREATE TABLE squad_runs (
                id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES squad_tasks(id),
                status TEXT NOT NULL, workflow_path TEXT, workflow_state_path TEXT,
                session_id TEXT, started_at TEXT NOT NULL, finished_at TEXT, error TEXT
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO squad_tasks (id, name, description, repo_scope, mount_scope,
                 interval_secs, created_at, updated_at)
             VALUES (?1, 'alpha', 'd', 'workspace', 'directory', 60, ?2, ?2)",
            params![task_id, timestamp(now())],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO squad_runs (id, task_id, status, started_at)
             VALUES (?1, ?2, 'running', ?3)",
            params![run_id.as_str(), task_id, timestamp(now())],
        )
        .unwrap();
    }
    store.migrate().unwrap();
    store.migrate().unwrap();

    let run = only_run(&store);
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!((run.phase, run.image_warning), (None, None));
    assert_eq!(task_phase(&store), None);
    assert!(store
        .set_run_phase(&run_id, Some(RunPhase::Building))
        .unwrap());
    assert_eq!(only_run(&store).phase, Some(RunPhase::Building));
}

#[test]
fn a_phase_this_binary_does_not_know_reads_as_none() {
    let (store, _, _) = running();
    store
        .lock()
        .execute("UPDATE squad_runs SET phase = 'compacting'", [])
        .unwrap();
    assert_eq!(only_run(&store).phase, None);
    assert_eq!(only_run(&store).status, RunStatus::Running);
    assert_eq!(task_phase(&store), None);
}

#[test]
fn a_building_run_is_still_the_running_run() {
    let (store, task, run) = running();
    assert_eq!(task_phase(&store), None);
    assert!(store.set_run_phase(&run, Some(RunPhase::Building)).unwrap());

    let current = store.running_run_for(&task.id).unwrap().unwrap();
    assert_eq!(current.id, run);
    assert_eq!(current.status, RunStatus::Running);
    assert_eq!(current.phase, Some(RunPhase::Building));
    assert_eq!(task_phase(&store), Some(RunPhase::Building));
    let later = now() + chrono::Duration::hours(1);
    assert!(store.due_for_evaluation(later).unwrap().is_empty());

    assert!(store.set_run_phase(&run, None).unwrap());
    assert_eq!(only_run(&store).phase, None);
    assert_eq!(only_run(&store).status, RunStatus::Running);
    assert_eq!(task_phase(&store), None);
    // A due task reports the phase of its (finished) last run: none.
    store
        .finish_run(&run, RunStatus::NotTriggered, &Default::default(), now())
        .unwrap();
    assert_eq!(
        store.due_for_evaluation(later).unwrap()[0].last_run_phase,
        None
    );
}

#[test]
fn every_final_outcome_clears_the_phase_and_it_cannot_come_back() {
    for status in [
        RunStatus::NotTriggered,
        RunStatus::WorkflowExecuted,
        RunStatus::Failed,
        RunStatus::Canceled,
        RunStatus::Interrupted,
    ] {
        let (store, _, run) = running();
        store.set_run_phase(&run, Some(RunPhase::Building)).unwrap();
        assert!(store
            .finish_run(&run, status, &Default::default(), now())
            .unwrap());
        assert_eq!(only_run(&store).status, status);
        assert_eq!(only_run(&store).phase, None);
        // A report arriving after the run ended changes nothing.
        assert!(!store.set_run_phase(&run, Some(RunPhase::Building)).unwrap());
        assert_eq!(only_run(&store).phase, None);
        assert_eq!(task_phase(&store), None);
    }
}

#[test]
fn orphan_recovery_interrupts_a_building_run_and_clears_its_phase() {
    let (store, task, run) = running();
    store.set_run_phase(&run, Some(RunPhase::Building)).unwrap();
    assert_eq!(store.reconcile_orphaned_runs(now()).unwrap(), 1);
    let run = only_run(&store);
    assert_eq!(run.status, RunStatus::Interrupted);
    assert_eq!(run.phase, None);
    assert!(run.error.unwrap().contains("daemon restarted"));
    assert!(store.running_run_for(&task.id).unwrap().is_none());
}

#[test]
fn finishing_without_workflow_paths_keeps_the_recorded_ones() {
    for status in [RunStatus::Canceled, RunStatus::Failed] {
        let (store, _, run) = running();
        store
            .set_workflow_state_path(
                &run,
                Some(Path::new("workflow.toml")),
                Some(Path::new("state.json")),
            )
            .unwrap();
        store
            .finish_run(&run, status, &Default::default(), now())
            .unwrap();
        let run = only_run(&store);
        assert_eq!(
            run.workflow_path.as_deref(),
            Some(Path::new("workflow.toml"))
        );
        assert_eq!(
            run.workflow_state_path.as_deref(),
            Some(Path::new("state.json"))
        );
    }
    // Paths the final detail does carry are the ones recorded.
    let (store, _, run) = running();
    store
        .set_workflow_state_path(&run, Some(Path::new("old.toml")), None)
        .unwrap();
    let detail = RunDetail {
        workflow_path: Some("final.toml".into()),
        workflow_state_path: Some("final.json".into()),
        ..Default::default()
    };
    store
        .finish_run(&run, RunStatus::WorkflowExecuted, &detail, now())
        .unwrap();
    let run = only_run(&store);
    assert_eq!(run.workflow_path, detail.workflow_path);
    assert_eq!(run.workflow_state_path, detail.workflow_state_path);
}

#[test]
fn image_warnings_accumulate_once_each_and_outlive_the_run() {
    let (store, _, run) = running();
    for warning in ["base is stale", "agent is stale", "base is stale"] {
        store.set_run_image_warning(&run, warning).unwrap();
    }
    store
        .finish_run(&run, RunStatus::NotTriggered, &Default::default(), now())
        .unwrap();
    store.set_run_image_warning(&run, "too late").unwrap();
    assert_eq!(
        only_run(&store).image_warning.as_deref(),
        Some("base is stale\nagent is stale")
    );
}
