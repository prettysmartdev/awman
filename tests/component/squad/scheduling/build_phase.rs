//! The run's `building` phase (WI 0105), through the production scheduler and
//! the production `LocalTaskEvaluator` against the fake Docker CLI with its
//! private image store. A held `docker build` is the observable build; no
//! leader or host agent is ever launched.
use super::task;
use crate::support;
use awman::command::commands::mount_scope::MountScopeDecision;
use awman::command::commands::squad::evaluation::{
    LocalTaskEvaluator, SquadBuildLogs, SquadRunFrontends,
};
use awman::command::commands::squad::gateway::{LocalTaskGateway, TaskGateway};
use awman::command::commands::workflow_prerequisites::TaskPrerequisitePlan;
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::command::interaction::InteractionDecision;
use awman::data::clock::{Clock, ManualClock};
use awman::data::config::env::EnvSnapshot;
use awman::data::fs::task_store::{Run, RunPhase, RunStatus, TaskStore};
use awman::data::fs::{ApiPaths, AuthPathResolver, EnvPersistence, NoStore, SquadPaths};
use awman::data::ids::RunId;
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::engine::squad::env_state::{DaemonEnvState, Salt};
use awman::engine::squad::evaluator::{ImagePreparationEvent, RunProgress};
use awman::engine::squad::{EvaluationOutcome, EvaluationRequest, SquadScheduler, TaskEvaluator};
use awman_test_support::fakes::FakeCli;
use awman_test_support::TestSandbox;
use chrono::{TimeZone, Utc};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

const LIMIT: Duration = Duration::from_secs(30);
const NAME: &str = "builder";

struct Fixture {
    sandbox: TestSandbox,
    docker: FakeCli,
    env: EnvSnapshot,
    engines: Engines,
    paths: SquadPaths,
    store: Arc<TaskStore>,
    clock: Arc<ManualClock>,
    root: PathBuf,
}

/// One due default-workspace task with a Claude recipe and nothing built.
fn fixture(id: &'static str) -> Fixture {
    let mut sandbox = support::sandbox(id);
    let private = sandbox.root().to_path_buf();
    for key in ["TMPDIR", "TEMP", "TMP"] {
        sandbox.env_mut().set(key, &private);
    }
    let docker = sandbox.install_fake(support::fake_cli_exe(), "docker");
    awman_test_support::fakes::docker_cli_baseline(&docker);
    docker.enable_docker_image_store();
    docker.on(&["build"]).stdout("builder progress\n").exit(0);
    let env = support::env_snapshot(&sandbox);
    let runtime = Arc::new(ContainerRuntime::docker_in(env.clone()));
    let auth = AuthPathResolver::at_home(sandbox.home());
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()).with_env(env.clone()));
    let engines = Engines {
        runtime: runtime.clone(),
        container_runtime: Some(runtime.clone()),
        sandbox_runtime: None,
        git_engine: Arc::new(GitEngine::with_env(env.clone())),
        overlay_engine: overlay.clone(),
        auth_engine: Arc::new(AuthEngine::with_paths(
            auth,
            ApiPaths::from_env(&env).unwrap(),
        )),
        agent_engine: Arc::new(AgentEngine::new(overlay, runtime)),
        workflow_state_store: Arc::new(WorkflowStateStore::at_git_root(sandbox.work())),
        credential_monitor: None,
        global_config: Arc::new(Default::default()),
        ephemeral_secrets: Arc::new(Default::default()),
        lease_workflow_review: None,
    };
    let paths = SquadPaths::from_env(&env).unwrap();
    let root = paths.task_dir(NAME).unwrap();
    std::fs::create_dir_all(root.join(".awman")).unwrap();
    std::fs::write(
        root.join(".awman/Dockerfile.claude"),
        "FROM awman-ignored\nRUN echo agent\n",
    )
    .unwrap();
    let store = Arc::new(TaskStore::open(&sandbox.work().join("tasks.db")).unwrap());
    store.migrate().unwrap();
    let clock = Arc::new(ManualClock::new(
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
    ));
    let mut row = task(
        NAME,
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        root.clone(),
    );
    row.agent = Some("claude".into());
    store.create(&row).unwrap();
    Fixture {
        sandbox,
        docker,
        env,
        engines,
        paths,
        store,
        clock,
        root,
    }
}

impl Fixture {
    fn scheduler(&self, evaluator: Arc<dyn TaskEvaluator>) -> Arc<SquadScheduler> {
        Arc::new(
            SquadScheduler::new(
                self.store.clone(),
                self.paths.clone(),
                evaluator,
                self.env.clone(),
            )
            .with_clock(self.clock.clone())
            .with_runtime(self.engines.runtime.clone()),
        )
    }

    fn production_evaluator(&self) -> Arc<LocalTaskEvaluator> {
        Arc::new(LocalTaskEvaluator::new(
            self.engines.clone(),
            self.env.clone(),
            Arc::new(NoFrontends),
        ))
    }

    fn latest(&self) -> Run {
        self.store.runs_for(NAME, 1).unwrap().remove(0)
    }

    fn task_phase(&self) -> Option<RunPhase> {
        self.store.list().unwrap()[0].last_run_phase
    }

    fn builds(&self) -> usize {
        self.docker
            .calls()
            .iter()
            .filter(|c| c.argv.first().is_some_and(|a| a == "build"))
            .count()
    }

    fn hold_builds(&self) {
        self.docker.on(&["build"]).hold_until_released().exit(0);
    }

    /// Wait, without blocking a runtime worker, for a held build.
    async fn build_held(&self) {
        let docker = self.docker.clone();
        tokio::task::spawn_blocking(move || docker.wait_until_held(LIMIT))
            .await
            .unwrap();
    }

    /// Let held builds finish and wait for every build process to exit.
    async fn release_builds(&self) {
        self.docker.release();
        let docker = self.docker.clone();
        tokio::task::spawn_blocking(move || {
            awman_test_support::wait_for("every fake build to exit", LIMIT, || {
                docker
                    .calls()
                    .iter()
                    .filter(|c| c.argv.first().is_some_and(|a| a == "build"))
                    .all(|c| c.exit.is_some())
                    .then_some(())
            })
        })
        .await
        .unwrap();
    }
}

struct NoFrontends;
impl SquadRunFrontends for NoFrontends {
    fn leader_frontend(
        &self,
        _: &str,
        _: &RunId,
        _: &Path,
        _: &str,
        _: MountScopeDecision,
        _: InteractionDecision,
    ) -> Result<Box<dyn awman::engine::agent_runtime::frontend::AgentFrontend>, CommandError> {
        panic!("these runs end before a leader is launched")
    }
    fn workflow_frontend(
        &self,
        _: &str,
        _: &RunId,
        _: &Path,
        _: MountScopeDecision,
        _: InteractionDecision,
    ) -> Result<
        Box<dyn awman::command::commands::exec_workflow::ExecWorkflowCommandFrontend>,
        CommandError,
    > {
        panic!("these runs execute no workflow")
    }
}

struct Quiet;
impl UserMessageSink for Quiet {
    fn write_message(&mut self, _: UserMessage) {}
    fn replay_queued(&mut self) {}
}

/// One preparation report and the phase the run row had after it.
type Report = (ImagePreparationEvent, Option<RunPhase>);
type Seen = Arc<Mutex<Vec<Report>>>;

/// Forwards every report to the scheduler's own recorder, then notes what the
/// run row says after it: each phase the run ever had.
struct Observed {
    scheduler: Arc<dyn RunProgress>,
    store: Arc<TaskStore>,
    seen: Seen,
}
impl RunProgress for Observed {
    fn workflow_started(&self, run: &RunId, workflow: &Path, state: &Path) {
        self.scheduler.workflow_started(run, workflow, state);
    }
    fn image_preparation(&self, run: &RunId, event: ImagePreparationEvent) {
        self.scheduler.image_preparation(run, event.clone());
        let phase = self.store.runs_for(NAME, 1).unwrap()[0].phase;
        self.seen.lock().unwrap().push((event, phase));
    }
}

/// A run's production image preparation, held open afterwards where the leader
/// would be running.
struct PrepareThenHold {
    production: Arc<LocalTaskEvaluator>,
    engines: Engines,
    env: EnvSnapshot,
    paths: SquadPaths,
    store: Arc<TaskStore>,
    seen: Seen,
    prepared: Semaphore,
    finish: Semaphore,
}

impl PrepareThenHold {
    fn new(f: &Fixture) -> Arc<Self> {
        Arc::new(Self {
            production: f.production_evaluator(),
            engines: f.engines.clone(),
            env: f.env.clone(),
            paths: f.paths.clone(),
            store: f.store.clone(),
            seen: Default::default(),
            prepared: Semaphore::new(0),
            finish: Semaphore::new(0),
        })
    }

    async fn prepared(&self) {
        tokio::time::timeout(LIMIT, self.prepared.acquire())
            .await
            .expect("preparation did not finish")
            .unwrap()
            .forget();
    }

    fn take(&self) -> Vec<Report> {
        std::mem::take(&mut self.seen.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl TaskEvaluator for PrepareThenHold {
    async fn evaluate(&self, mut request: EvaluationRequest) -> EvaluationOutcome {
        request.progress = Arc::new(Observed {
            scheduler: request.progress.clone(),
            store: self.store.clone(),
            seen: self.seen.clone(),
        });
        let context = self.paths.task_image_context(&request.task.name).unwrap();
        let mut session = TaskPrerequisitePlan::for_task(&request.task)
            .open_session(&self.engines, Some(self.env.clone()))
            .unwrap();
        let mut logs = SquadBuildLogs::new(&self.env, &request.task.name, &request.run_id)
            .unwrap()
            .with_progress(request.progress.clone());
        let prepared = self
            .production
            .prepare_run_images(&request, &mut session, &context, &mut Quiet, &mut logs)
            .await;
        self.prepared.add_permits(1);
        self.finish.acquire().await.unwrap().forget();
        match prepared {
            Ok(_) => EvaluationOutcome::NotTriggered { reason: None },
            Err(error) => EvaluationOutcome::Failed {
                error: error.to_string(),
                reason: None,
            },
        }
    }
}

fn gateway(f: &Fixture, scheduler: &SquadScheduler) -> LocalTaskGateway {
    LocalTaskGateway::new(
        f.store.clone(),
        f.engines.clone(),
        scheduler.status_handle(),
        f.paths.clone(),
        Arc::new(DaemonEnvState::new(
            Box::new(NoStore),
            EnvPersistence::None,
            Salt::from_hex(&"ab".repeat(32)).unwrap(),
        )),
    )
    .with_env(f.env.clone())
}

fn tick(scheduler: &Arc<SquadScheduler>) -> tokio::task::JoinHandle<()> {
    let scheduler = scheduler.clone();
    tokio::spawn(async move { scheduler.evaluate_due().await })
}

async fn ended(tick: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(LIMIT, tick)
        .await
        .expect("the evaluation did not end")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_actual_build_is_the_phase_of_the_current_run_and_a_reuse_never_is() {
    let f = fixture("squad::scheduling::build_phase::an_actual_build_is_the_phase_of_the_current_run_and_a_reuse_never_is");
    f.hold_builds();
    let evaluator = PrepareThenHold::new(&f);
    let scheduler = f.scheduler(evaluator.clone());
    let gateway = gateway(&f, &scheduler);

    let run = tick(&scheduler);
    f.build_held().await;
    let building = f.latest();
    assert_eq!(building.status, RunStatus::Running);
    assert_eq!(building.phase, Some(RunPhase::Building));
    assert_eq!(f.task_phase(), Some(RunPhase::Building));
    // Still the current run: what attach admission reads, what a new run
    // request reuses, and what keeps the task from being selected again.
    let current = gateway.runs(NAME, 1).await.unwrap().remove(0);
    assert_eq!(current.id, building.id);
    assert_eq!(current.status, RunStatus::Running);
    assert_eq!(current.phase, Some(RunPhase::Building));
    assert_eq!(gateway.create_run(NAME).await.unwrap(), building.id);
    assert_eq!(
        gateway.get(NAME).await.unwrap().last_run_phase,
        Some(RunPhase::Building)
    );

    f.release_builds().await;
    evaluator.prepared().await;
    // The same run reads `running` again while it carries on.
    assert_eq!(f.latest().status, RunStatus::Running);
    assert_eq!(f.latest().phase, None);
    assert_eq!(f.task_phase(), None);
    let seen = evaluator.take();
    let (last, during) = seen.split_last().unwrap();
    assert_eq!(during.len(), 2, "project and agent builds: {seen:?}");
    assert!(during.iter().all(|(event, phase)| matches!(
        event,
        ImagePreparationEvent::BuildStarted { .. }
    ) && *phase == Some(RunPhase::Building)));
    assert_eq!(*last, (ImagePreparationEvent::PreparationFinished, None));
    evaluator.finish.add_permits(1);
    ended(run).await;
    assert_eq!(f.latest().status, RunStatus::NotTriggered);
    assert_eq!(f.latest().phase, None);

    // The next run reuses both images: it never has a phase.
    f.clock.advance(Duration::from_secs(120));
    let before = f.builds();
    let run = tick(&scheduler);
    evaluator.prepared().await;
    assert_eq!(f.latest().status, RunStatus::Running);
    assert_eq!(f.latest().phase, None);
    assert_eq!(f.builds(), before, "unchanged inputs are reused");
    assert_eq!(
        evaluator.take(),
        [(ImagePreparationEvent::PreparationFinished, None)]
    );
    evaluator.finish.add_permits(1);
    ended(run).await;
    assert_eq!(f.latest().status, RunStatus::NotTriggered);
    f.sandbox.finish();
}

#[tokio::test]
async fn historical_workflow_lookup_is_pinned_to_the_task_and_run_row() {
    use awman::data::fs::{RunWorkflow, RunWorkflowUnavailable};
    use awman::data::ids::TaskId;
    let f = fixture("squad::scheduling::build_phase::historical_workflow_lookup_is_pinned_to_the_task_and_run_row");
    let scheduler = f.scheduler(f.production_evaluator());
    let gateway = gateway(&f, &scheduler);
    let task = f.store.get(NAME).unwrap().unwrap();
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

    let first = f.store.start_run(&task.id, None, at).unwrap();
    // Saved where the state store puts them: `<root>/.awman/workflows/`.
    let first_path = f.sandbox.work().join("first/.awman/workflows/state.json");
    std::fs::create_dir_all(first_path.parent().unwrap()).unwrap();
    std::fs::write(
        &first_path,
        serde_json::json!({
            "schema_version": 4, "workflow_name": "first-flow", "workflow_hash": "h",
            "step_states": {"alpha": "Succeeded"}, "completed_steps": ["alpha"],
            "current_step_index": 0, "started_at": at, "updated_at": at,
            "steps": [{"name":"alpha", "depends_on": []}]
        })
        .to_string(),
    )
    .unwrap();
    f.store
        .set_workflow_state_path(&first, None, Some(&first_path))
        .unwrap();

    let second = f
        .store
        .start_run(&task.id, None, at + chrono::Duration::minutes(2))
        .unwrap();
    let second_path = f.sandbox.work().join("second/.awman/workflows/state.json");
    std::fs::create_dir_all(second_path.parent().unwrap()).unwrap();
    let second_at = at + chrono::Duration::minutes(2);
    std::fs::write(
        &second_path,
        serde_json::json!({
            "schema_version": 4, "workflow_name": "second-flow", "workflow_hash": "h",
            "step_states": {"beta": "Pending"}, "completed_steps": [],
            "current_step_index": 0, "started_at": second_at, "updated_at": second_at,
            "steps": [{"name":"beta", "depends_on": []}]
        })
        .to_string(),
    )
    .unwrap();
    f.store
        .set_workflow_state_path(&second, None, Some(&second_path))
        .unwrap();

    let first_view = gateway.run_workflow(NAME, &first).await.unwrap();
    let second_view = gateway.run_workflow(NAME, &second).await.unwrap();
    assert!(
        matches!(first_view, RunWorkflow::Dag(dag) if dag.workflow_name == "first-flow" && dag.nodes[0].name == "alpha")
    );
    assert!(
        matches!(second_view, RunWorkflow::Dag(dag) if dag.workflow_name == "second-flow" && dag.nodes[0].name == "beta")
    );

    let no_workflow = f
        .store
        .start_run(&task.id, None, at + chrono::Duration::minutes(4))
        .unwrap();
    assert_eq!(
        gateway.run_workflow(NAME, &no_workflow).await.unwrap(),
        RunWorkflow::NoWorkflow
    );
    let missing = f
        .store
        .start_run(&task.id, None, at + chrono::Duration::minutes(6))
        .unwrap();
    let missing_path = f.sandbox.work().join("pruned/.awman/workflows/state.json");
    f.store
        .set_workflow_state_path(&missing, None, Some(&missing_path))
        .unwrap();
    assert_eq!(
        gateway.run_workflow(NAME, &missing).await.unwrap(),
        RunWorkflow::Unavailable {
            reason: RunWorkflowUnavailable::Missing
        }
    );

    let mut other = task;
    other.id = TaskId::new();
    other.name = "other-owner".into();
    f.store.create(&other).unwrap();
    assert!(gateway.run_workflow("other-owner", &first).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_stale_image_is_recorded_on_the_run_that_used_it() {
    let f = fixture(
        "squad::scheduling::build_phase::a_kept_stale_image_is_recorded_on_the_run_that_used_it",
    );
    let evaluator = PrepareThenHold::new(&f);
    let scheduler = f.scheduler(evaluator.clone());
    evaluator.finish.add_permits(2);
    ended(tick(&scheduler)).await;
    assert_eq!(f.latest().image_warning, None);
    evaluator.take();

    f.docker
        .on(&["build"])
        .stderr("ERROR: RUN false exited 1\n")
        .exit(1);
    std::fs::write(f.root.join("Dockerfile.dev"), "FROM scratch\nRUN false\n").unwrap();
    f.clock.advance(Duration::from_secs(120));
    ended(tick(&scheduler)).await;

    let run = f.latest();
    assert_eq!(run.status, RunStatus::NotTriggered, "{:?}", run.error);
    assert_eq!(run.phase, None);
    let warning = run.image_warning.expect("stale warning on the run");
    assert!(warning.contains("squad-builder"), "{warning}");
    assert!(warning.contains("rebuild failed"), "{warning}");
    let seen = evaluator.take();
    assert!(
        seen.iter()
            .any(|(_, phase)| *phase == Some(RunPhase::Building)),
        "a failed rebuild was still a build: {seen:?}"
    );
    assert_eq!(
        seen.last(),
        Some(&(ImagePreparationEvent::PreparationFinished, None))
    );
    f.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_build_ends_the_run_failed_naming_its_build_log() {
    let f = fixture(
        "squad::scheduling::build_phase::a_failed_build_ends_the_run_failed_naming_its_build_log",
    );
    f.docker
        .on(&["build"])
        .stderr("ERROR: no space left\n")
        .exit(1);
    ended(tick(&f.scheduler(f.production_evaluator()))).await;

    assert_eq!(f.builds(), 1);
    let run = f.latest();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.phase, None);
    assert_eq!(f.task_phase(), None);
    let error = run.error.unwrap();
    assert!(error.contains("failed to build project image"), "{error}");
    assert!(error.contains("build log:"), "{error}");
    assert!(f.store.get(NAME).unwrap().unwrap().backoff_until.is_some());
    f.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
/// hygiene-allow(H3): proving a killed builder never finishes after its hold is released needs real elapsed time; a process that no longer exists emits no event to wait for.
async fn canceling_a_building_run_records_canceled_without_a_phase() {
    let f = fixture(
        "squad::scheduling::build_phase::canceling_a_building_run_records_canceled_without_a_phase",
    );
    f.hold_builds();
    let scheduler = f.scheduler(f.production_evaluator());
    let gateway = gateway(&f, &scheduler);
    let run = tick(&scheduler);
    f.build_held().await;
    assert_eq!(f.latest().phase, Some(RunPhase::Building));
    let before = f.docker.calls().len();

    gateway.cancel(NAME).await.unwrap();
    // The run ends while the build process is still held.
    ended(run).await;
    let canceled = f.latest();
    assert_eq!(canceled.status, RunStatus::Canceled);
    assert_eq!(canceled.phase, None);
    assert_eq!(canceled.error, None);
    assert_eq!(f.task_phase(), None);
    assert!(f.store.get(NAME).unwrap().unwrap().backoff_until.is_none());
    assert!(
        f.docker.calls()[before..]
            .iter()
            .any(|c| c.argv.first().is_some_and(|a| a == "ps")),
        "the task's containers are looked up to be stopped"
    );
    assert_eq!(scheduler.status_handle().lock().unwrap().in_flight, 0);

    // The held builder was killed with the run: releasing the hold now lets
    // no build finish, so nothing can be published after the cancel.
    f.docker.release();
    std::thread::sleep(Duration::from_millis(500));
    let finished: Vec<_> = f
        .docker
        .calls()
        .into_iter()
        .filter(|c| c.argv.first().is_some_and(|a| a == "build"))
        .filter_map(|c| c.exit)
        .collect();
    assert!(
        finished.is_empty(),
        "a cancelled build ran on: {finished:?}"
    );
    let published = std::fs::read_dir(f.docker.dir().join("images"))
        .map(|dir| dir.count())
        .unwrap_or(0);
    assert_eq!(published, 0, "no image is published after the cancel");
    assert_eq!(f.latest().status, RunStatus::Canceled);
    assert_eq!(f.latest().phase, None);
    f.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_recovery_interrupts_a_building_run_for_good() {
    let f = fixture(
        "squad::scheduling::build_phase::restart_recovery_interrupts_a_building_run_for_good",
    );
    f.hold_builds();
    let evaluator = PrepareThenHold::new(&f);
    let run = tick(&f.scheduler(evaluator.clone()));
    f.build_held().await;
    assert_eq!(f.latest().phase, Some(RunPhase::Building));

    // What a starting daemon does with the rows the previous one left.
    let restarted = TaskStore::open(&f.sandbox.work().join("tasks.db")).unwrap();
    restarted.migrate().unwrap();
    assert_eq!(
        restarted
            .reconcile_orphaned_runs(f.clock.now_utc())
            .unwrap(),
        1
    );
    let interrupted = f.latest();
    assert_eq!(interrupted.status, RunStatus::Interrupted);
    assert_eq!(interrupted.phase, None);
    assert_eq!(f.task_phase(), None);

    // Late reports from the old evaluation cannot reopen or relabel the run.
    f.release_builds().await;
    evaluator.prepared().await;
    evaluator.finish.add_permits(1);
    ended(run).await;
    let seen = evaluator.take();
    assert_eq!(seen[0].1, Some(RunPhase::Building), "{seen:?}");
    assert!(
        seen.len() > 1 && seen[1..].iter().all(|(_, phase)| phase.is_none()),
        "{seen:?}"
    );
    assert_eq!(f.latest().status, RunStatus::Interrupted);
    assert_eq!(f.latest().phase, None);
    f.sandbox.finish();
}
