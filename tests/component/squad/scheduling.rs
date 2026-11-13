use crate::support;
use awman::command::commands::{RepairDecision, WorkflowRepairLoop};
use awman::data::{
    clock::{Clock, ManualClock},
    fs::{MountScope, RunStatus, SquadPaths, Task, TaskStatus, TaskStore},
};
use awman::engine::squad::{EvaluationOutcome, EvaluationRequest, SquadScheduler, TaskEvaluator};
use chrono::{TimeZone, Utc};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Evaluator {
    calls: Mutex<Vec<String>>,
    fail: bool,
}
#[async_trait::async_trait]
impl TaskEvaluator for Evaluator {
    async fn evaluate(&self, request: EvaluationRequest) -> EvaluationOutcome {
        self.calls.lock().unwrap().push(request.task.name);
        if self.fail {
            let mut repair =
                WorkflowRepairLoop::new(request.task_dir.join("workflow.toml"), "leader prompt");
            for attempt in 1..=3 {
                assert!(
                    matches!(repair.record(Err(format!("invalid attempt {attempt}"))),RepairDecision::Retry{attempt:n,..} if n==attempt)
                );
            }
            let RepairDecision::Exhausted(error) = repair.record(Err("still invalid".into()))
            else {
                panic!("unbounded repair")
            };
            EvaluationOutcome::Failed {
                error,
                reason: None,
            }
        } else {
            EvaluationOutcome::NotTriggered {
                reason: Some("condition absent".into()),
            }
        }
    }
}
fn task(name: &str, now: chrono::DateTime<Utc>, scope: std::path::PathBuf) -> Task {
    Task {
        id: awman::data::ids::TaskId::new(),
        name: name.into(),
        description: "evaluate".into(),
        repo_scope: scope,
        mount_scope: MountScope::Directory,
        overlays: vec![],
        interval_secs: 60,
        status: TaskStatus::Active,
        agent: None,
        model: None,
        backoff_until: None,
        created_at: now,
        updated_at: now,
        last_run_at: None,
        trigger_requested_at: None,
        last_run_status: None,
        unmet_env: vec![],
        prerequisites: None,
    }
}

#[test]
fn paused_failure_does_not_mask_live_squad_health() {
    use awman::engine::squad::{SquadHealth, SquadSupervisor};

    let mut paused = task("paused", Utc::now(), Default::default());
    paused.status = TaskStatus::Paused;
    paused.last_run_status = Some(RunStatus::Failed);
    assert_eq!(
        SquadSupervisor::health_of_running(Some(&[paused.clone()])),
        SquadHealth::Healthy
    );
    let mut active = task("active", Utc::now(), Default::default());
    active.last_run_status = Some(RunStatus::Running);
    assert_eq!(
        SquadSupervisor::health_of_running(Some(&[paused.clone(), active.clone()])),
        SquadHealth::Running
    );
    active.unmet_env = vec!["MISSING_TOKEN".into()];
    assert_eq!(
        SquadSupervisor::health_of_running(Some(&[paused.clone(), active.clone()])),
        SquadHealth::EnvUnmet
    );
    paused.status = TaskStatus::Active;
    assert_eq!(
        SquadSupervisor::health_of_running(Some(&[paused, active])),
        SquadHealth::Failed
    );
    assert_eq!(
        SquadSupervisor::health_of_running(None),
        SquadHealth::Unreachable
    );
}
/// Guards: 204cea73. Due eligibility uses the injected clock, so slow runners
/// cannot move the backoff boundary or change the expected repair count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_exhaustion_records_failure_and_backoff_advances_only_with_clock() {
    let sandbox = support::sandbox(
        "squad::scheduling::repair_exhaustion_records_failure_and_backoff_advances_only_with_clock",
    );
    let store = Arc::new(TaskStore::open(&sandbox.work().join("tasks.db")).unwrap());
    store.migrate().unwrap();
    let clock = Arc::new(ManualClock::new(
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
    ));
    store
        .create(&task(
            "repair",
            clock.now_utc(),
            sandbox.work().to_path_buf(),
        ))
        .unwrap();
    let evaluator = Arc::new(Evaluator {
        calls: Default::default(),
        fail: true,
    });
    let scheduler = SquadScheduler::new(
        store.clone(),
        SquadPaths::from_root(sandbox.awman_home()),
        evaluator.clone(),
        support::env_snapshot(&sandbox),
    )
    .with_clock(clock.clone());
    scheduler.evaluate_due().await;
    let row = store.get("repair").unwrap().unwrap();
    assert_eq!(
        row.backoff_until,
        Some(clock.now_utc() + chrono::Duration::seconds(120))
    );
    let runs = store.runs_for("repair", 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Failed);
    scheduler.evaluate_due().await;
    assert_eq!(evaluator.calls.lock().unwrap().len(), 1);
    clock.advance(Duration::from_secs(120));
    scheduler.evaluate_due().await;
    assert_eq!(evaluator.calls.lock().unwrap().len(), 2);
    assert_eq!(
        store.get("repair").unwrap().unwrap().backoff_until,
        Some(clock.now_utc() + chrono::Duration::seconds(240))
    );
    assert_eq!(scheduler.status_handle().lock().unwrap().in_flight, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_config_admits_nothing_then_recovers_without_restart() {
    let sandbox = support::sandbox(
        "squad::scheduling::malformed_config_admits_nothing_then_recovers_without_restart",
    );
    let env = support::env_snapshot(&sandbox);
    let path = awman::data::config::file::global_config_path(&env).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{broken").unwrap();
    let store = Arc::new(TaskStore::open(&sandbox.work().join("tasks.db")).unwrap());
    store.migrate().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    store
        .create(&task("recover", now, sandbox.work().to_path_buf()))
        .unwrap();
    let evaluator = Arc::new(Evaluator {
        calls: Default::default(),
        fail: false,
    });
    let scheduler = SquadScheduler::new(
        store.clone(),
        SquadPaths::from_root(sandbox.awman_home()),
        evaluator.clone(),
        env,
    )
    .with_clock(Arc::new(ManualClock::new(now)));
    scheduler.evaluate_due().await;
    assert!(evaluator.calls.lock().unwrap().is_empty());
    assert!(scheduler
        .status_handle()
        .lock()
        .unwrap()
        .config_error
        .is_some());
    std::fs::write(&path, "{}").unwrap();
    scheduler.evaluate_due().await;
    assert_eq!(*evaluator.calls.lock().unwrap(), vec!["recover"]);
    assert!(scheduler
        .status_handle()
        .lock()
        .unwrap()
        .config_error
        .is_none());
    assert_eq!(
        store.runs_for("recover", 10).unwrap()[0].status,
        RunStatus::NotTriggered
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_ticks_share_capacity_and_cancelled_evaluations_leave_no_running_rows() {
    struct Held {
        entered: tokio::sync::mpsc::UnboundedSender<String>,
        release: tokio::sync::Semaphore,
    }
    #[async_trait::async_trait]
    impl TaskEvaluator for Held {
        async fn evaluate(&self, request: EvaluationRequest) -> EvaluationOutcome {
            self.entered.send(request.task.name).unwrap();
            self.release.acquire().await.unwrap().forget();
            EvaluationOutcome::NotTriggered { reason: None }
        }
    }
    let sandbox=support::sandbox("squad::scheduling::concurrent_ticks_share_capacity_and_cancelled_evaluations_leave_no_running_rows");
    let store = Arc::new(TaskStore::open(&sandbox.work().join("tasks.db")).unwrap());
    store.migrate().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    for name in ["one", "two", "three", "four"] {
        store
            .create(&task(name, now, sandbox.work().to_path_buf()))
            .unwrap();
    }
    let (tx, mut entered) = tokio::sync::mpsc::unbounded_channel();
    let evaluator = Arc::new(Held {
        entered: tx,
        release: tokio::sync::Semaphore::new(0),
    });
    let scheduler = Arc::new(
        SquadScheduler::new(
            store.clone(),
            SquadPaths::from_root(sandbox.awman_home()),
            evaluator.clone(),
            support::env_snapshot(&sandbox),
        )
        .with_clock(Arc::new(ManualClock::new(now))),
    );
    let run = {
        let scheduler = scheduler.clone();
        tokio::spawn(async move { scheduler.evaluate_due().await })
    };
    let mut finished = Vec::new();
    for _ in 0..2 {
        finished.push(
            tokio::time::timeout(Duration::from_secs(5), entered.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(scheduler.status_handle().lock().unwrap().in_flight, 2);
    scheduler.evaluate_due().await;
    assert!(
        entered.try_recv().is_err(),
        "a concurrent tick exceeded the global cap"
    );
    evaluator.release.add_permits(2);
    run.await.unwrap();
    for name in finished {
        assert_eq!(
            store.runs_for(&name, 10).unwrap()[0].status,
            RunStatus::NotTriggered
        );
    }
    let run = {
        let scheduler = scheduler.clone();
        tokio::spawn(async move { scheduler.evaluate_due().await })
    };
    let mut cancelled = Vec::new();
    for _ in 0..2 {
        cancelled.push(
            tokio::time::timeout(Duration::from_secs(5), entered.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    for name in &cancelled {
        let id = store.get(name).unwrap().unwrap().id;
        assert!(scheduler
            .status_handle()
            .lock()
            .unwrap()
            .cancel_run(id.as_str())
            .is_some());
    }
    run.await.unwrap();
    for name in cancelled {
        assert_eq!(
            store.runs_for(&name, 10).unwrap()[0].status,
            RunStatus::Canceled
        );
    }
    let status = scheduler.status_handle();
    let status = status.lock().unwrap();
    assert_eq!(status.in_flight, 0);
    assert!(status.cancellations.is_empty());
    drop(status);
}
