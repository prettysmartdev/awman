//! Squad directory-task image inputs and per-run freshness (WI 0105).
//!
//! Drives the production `LocalTaskEvaluator::prepare_run_images` against
//! the fake Docker CLI with its private image store (builds publish tag,
//! labels and a new id), with private sandbox state only. No leader is
//! launched: preparation is the part of a run that happens before one.
use crate::support;
use awman::command::commands::mount_scope::MountScopeDecision;
use awman::command::commands::squad::evaluation::{
    LocalTaskEvaluator, PreparedImages, PreparedOutcome, SquadBuildLogs, SquadRunFrontends,
};
use awman::command::dispatch::Engines;
use awman::command::error::CommandError;
use awman::command::interaction::InteractionDecision;
use awman::data::config::env::EnvSnapshot;
use awman::data::fs::squad_paths::TaskImageContext;
use awman::data::fs::task_store::{MountScope, Task, TaskStatus};
use awman::data::fs::{ApiPaths, AuthPathResolver, SquadPaths};
use awman::data::ids::{RunId, TaskId};
use awman::data::image_tags::{agent_image_tag, project_image_tag};
use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::WorkflowStateStore;
use awman::engine::agent::AgentEngine;
use awman::engine::auth::AuthEngine;
use awman::engine::container::ContainerRuntime;
use awman::engine::git::GitEngine;
use awman::engine::overlay::OverlayEngine;
use awman::engine::squad::evaluator::{EvaluationRequest, ImagePreparationEvent, RunProgress};
use awman_test_support::fakes::FakeCli;
use awman_test_support::TestSandbox;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RECIPE: &str = "FROM awman-ignored\nRUN echo agent\n";

struct Fixture {
    sandbox: TestSandbox,
    docker: FakeCli,
    env: EnvSnapshot,
    engines: Engines,
    paths: SquadPaths,
}

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
    Fixture {
        sandbox,
        docker,
        env,
        engines,
        paths,
    }
}

fn task(name: &str, root: &Path) -> Task {
    let now = chrono::Utc::now();
    Task {
        id: TaskId::new(),
        name: name.into(),
        description: "d".into(),
        repo_scope: root.to_path_buf(),
        mount_scope: MountScope::Directory,
        overlays: vec![],
        interval_secs: 3600,
        status: TaskStatus::Active,
        agent: Some("claude".into()),
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

#[derive(Default)]
struct Recorder(Mutex<Vec<ImagePreparationEvent>>);
impl RunProgress for Recorder {
    fn workflow_started(&self, _: &RunId, _: &Path, _: &Path) {}
    fn image_preparation(&self, _: &RunId, event: ImagePreparationEvent) {
        self.0.lock().unwrap().push(event);
    }
}
impl Recorder {
    fn take(&self) -> Vec<ImagePreparationEvent> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

#[derive(Default)]
struct Messages(Vec<String>);
impl UserMessageSink for Messages {
    fn write_message(&mut self, message: UserMessage) {
        self.0.push(message.text);
    }
    fn replay_queued(&mut self) {}
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
        panic!("image preparation launches no leader")
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
        panic!("image preparation runs no workflow")
    }
}

struct Run {
    prepared: Result<Option<PreparedImages>, CommandError>,
    session: awman::data::session::Session,
    messages: Vec<String>,
    events: Vec<ImagePreparationEvent>,
}

/// One run's image preparation for `task`, exactly as `evaluate` performs it.
async fn prepare(f: &Fixture, task: &Task) -> Run {
    let context = f.paths.task_image_context(&task.name).unwrap();
    prepare_with(f, task, &context).await
}

async fn prepare_with(f: &Fixture, task: &Task, context: &TaskImageContext) -> Run {
    let recorder = Arc::new(Recorder::default());
    let run_id = RunId::new();
    let run_log_dir = f.sandbox.work().join(format!("runs/{}", run_id.as_str()));
    std::fs::create_dir_all(&run_log_dir).unwrap();
    let request = EvaluationRequest {
        task: task.clone(),
        run_id: run_id.clone(),
        task_dir: f.paths.task_dir(&task.name).unwrap(),
        run_log_dir,
        guidance: None,
        agents_to_models: None,
        default_leader: None,
        progress: recorder.clone(),
    };
    let mut session =
        awman::command::commands::workflow_prerequisites::TaskPrerequisitePlan::for_task(task)
            .open_session(&f.engines, Some(f.env.clone()))
            .unwrap();
    let evaluator =
        LocalTaskEvaluator::new(f.engines.clone(), f.env.clone(), Arc::new(NoFrontends));
    let mut sink = Messages::default();
    let mut logs = SquadBuildLogs::new(&f.env, &task.name, &run_id)
        .unwrap()
        .with_progress(recorder.clone());
    let prepared = evaluator
        .prepare_run_images(&request, &mut session, context, &mut sink, &mut logs)
        .await;
    Run {
        prepared,
        session,
        messages: sink.0,
        events: recorder.take(),
    }
}

fn builds(docker: &FakeCli, since: usize) -> Vec<Vec<String>> {
    docker
        .calls()
        .into_iter()
        .skip(since)
        .filter(|c| c.argv.first().is_some_and(|a| a == "build"))
        .map(|c| c.argv)
        .collect()
}

fn tag_of(argv: &[String]) -> String {
    argv.windows(2)
        .find(|w| w[0] == "-t")
        .map(|w| w[1].clone())
        .unwrap()
}

fn started(events: &[ImagePreparationEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            ImagePreparationEvent::BuildStarted { image } => Some(image.clone()),
            _ => None,
        })
        .collect()
}

fn finished_once(events: &[ImagePreparationEvent]) {
    assert_eq!(
        events
            .iter()
            .filter(|e| **e == ImagePreparationEvent::PreparationFinished)
            .count(),
        1,
        "{events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&ImagePreparationEvent::PreparationFinished)
    );
}

fn seed_default(f: &Fixture, name: &str) -> PathBuf {
    let root = f.paths.task_dir(name).unwrap();
    std::fs::create_dir_all(root.join(".awman")).unwrap();
    std::fs::write(root.join(".awman/Dockerfile.claude"), RECIPE).unwrap();
    root
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_inputs_reuse_and_changed_recipes_rebuild_exactly_their_dependents() {
    let f = fixture("engine::image_prep::squad_image_inputs::unchanged_inputs_reuse_and_changed_recipes_rebuild_exactly_their_dependents");
    let root = seed_default(&f, "alpha");
    let task = task("alpha", &root);
    let project = project_image_tag(&root);
    let claude = agent_image_tag(&root, "claude");
    assert!(project.contains("squad-alpha"), "{project}");

    // First run: nothing installed, both built from the task context.
    let first = prepare(&f, &task).await;
    let prepared = first.prepared.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(prepared.project.outcome, PreparedOutcome::Built);
    assert_eq!(prepared.agents["claude"].outcome, PreparedOutcome::Built);
    assert!(root.join("Dockerfile.dev").is_file(), "scaffolded");
    let calls = builds(&f.docker, 0);
    assert_eq!(
        calls.iter().map(|a| tag_of(a)).collect::<Vec<_>>(),
        [project.clone(), claude.clone()]
    );
    let canonical = std::fs::canonicalize(&root).unwrap();
    for argv in &calls {
        let context = PathBuf::from(argv.last().unwrap());
        assert_eq!(std::fs::canonicalize(context).unwrap(), canonical);
    }
    assert_eq!(started(&first.events), [project.clone(), claude.clone()]);
    finished_once(&first.events);
    // The run's session is pinned to exactly these tags.
    let binding = first.session.lease_images().unwrap();
    assert_eq!(binding.project, project);
    assert_eq!(binding.agents["claude"], claude);

    // Second run, unchanged inputs: zero builds, no build event.
    let before = f.docker.calls().len();
    let second = prepare(&f, &task).await;
    let again = second.prepared.unwrap().unwrap();
    assert!(builds(&f.docker, before).is_empty());
    assert!(started(&second.events).is_empty());
    finished_once(&second.events);
    assert_eq!(again.project.outcome, PreparedOutcome::Reused);
    assert_eq!(again.agents["claude"].outcome, PreparedOutcome::Reused);
    assert_eq!(again.project.image_id, prepared.project.image_id);

    // An agent recipe edit rebuilds only that agent.
    std::fs::write(
        root.join(".awman/Dockerfile.claude"),
        format!("{RECIPE}RUN echo more\n"),
    )
    .unwrap();
    let before = f.docker.calls().len();
    let third = prepare(&f, &task).await.prepared.unwrap().unwrap();
    assert_eq!(
        builds(&f.docker, before)
            .iter()
            .map(|a| tag_of(a))
            .collect::<Vec<_>>(),
        vec![claude.clone()]
    );
    assert_eq!(third.project.outcome, PreparedOutcome::Reused);
    assert_eq!(third.agents["claude"].outcome, PreparedOutcome::Built);

    // A base edit (or a tracked COPY input) rebuilds the base and its agents.
    std::fs::write(root.join("tool.sh"), "echo 1\n").unwrap();
    std::fs::write(
        root.join("Dockerfile.dev"),
        "FROM scratch\nCOPY tool.sh /tool.sh\n",
    )
    .unwrap();
    let before = f.docker.calls().len();
    let fourth = prepare(&f, &task).await.prepared.unwrap().unwrap();
    assert_eq!(
        builds(&f.docker, before)
            .iter()
            .map(|a| tag_of(a))
            .collect::<Vec<_>>(),
        [project.clone(), claude.clone()]
    );
    assert_ne!(fourth.project.image_id, third.project.image_id);
    std::fs::write(root.join("tool.sh"), "echo 2\n").unwrap();
    let before = f.docker.calls().len();
    prepare(&f, &task).await.prepared.unwrap().unwrap();
    assert_eq!(builds(&f.docker, before).len(), 2, "COPY input change");
    f.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_refresh_keeps_the_old_image_as_stale_and_a_missing_build_fails() {
    let f = fixture("engine::image_prep::squad_image_inputs::failed_refresh_keeps_the_old_image_as_stale_and_a_missing_build_fails");
    let root = seed_default(&f, "beta");
    let task = task("beta", &root);
    let project = project_image_tag(&root);
    let first = prepare(&f, &task).await.prepared.unwrap().unwrap();

    f.docker
        .on(&["build"])
        .stderr("ERROR: RUN false exited 1\n")
        .exit(1);
    std::fs::write(root.join("Dockerfile.dev"), "FROM scratch\nRUN false\n").unwrap();
    let run = prepare(&f, &task).await;
    let kept = run.prepared.unwrap().unwrap();
    assert_eq!(kept.project.outcome, PreparedOutcome::StaleKept);
    assert_eq!(
        kept.project.image_id, first.project.image_id,
        "old image kept"
    );
    assert_eq!(kept.warnings.len(), 1);
    assert!(run
        .events
        .iter()
        .any(|e| matches!(e, ImagePreparationEvent::Stale { image, .. } if *image == project)));
    finished_once(&run.events);
    let logs = f.paths.task_builds_dir("beta").unwrap();
    assert!(
        std::fs::read_dir(&logs).unwrap().count() >= 3,
        "per-task build logs"
    );
    assert!(
        !run.messages
            .iter()
            .any(|m| m.contains(&format!("Built image {project}"))),
        "a kept image is never reported as built: {:?}",
        run.messages
    );

    // A task whose images were never built has nothing to fall back to.
    let root = seed_default(&f, "gamma");
    let missing = prepare(&f, &task_named("gamma", &root)).await;
    let error = missing.prepared.unwrap_err().to_string();
    assert!(error.contains("failed to build project image"), "{error}");
    assert!(error.contains("build log:"), "{error}");
    finished_once(&missing.events);
    assert!(missing.session.lease_images().is_none());
    f.sandbox.finish();
}

fn task_named(name: &str, root: &Path) -> Task {
    task(name, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_non_git_task_migrates_legacy_recipes_once_and_never_builds_from_its_folder() {
    let f = fixture("engine::image_prep::squad_image_inputs::custom_non_git_task_migrates_legacy_recipes_once_and_never_builds_from_its_folder");
    // A chosen subdirectory of a larger checkout: still a directory task.
    let parent = f.sandbox.work().join("checkout");
    let custom = parent.join("nested/custom");
    std::fs::create_dir_all(custom.join(".awman")).unwrap();
    std::fs::create_dir_all(parent.join(".awman")).unwrap();
    std::fs::write(
        parent.join("Dockerfile.dev"),
        "FROM parent-must-not-be-used\n",
    )
    .unwrap();
    std::fs::write(
        custom.join("Dockerfile.dev"),
        "FROM scratch\nCOPY scripts/setup.sh /setup.sh\n",
    )
    .unwrap();
    std::fs::create_dir_all(custom.join("scripts")).unwrap();
    std::fs::write(custom.join("scripts/setup.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(custom.join(".awman/Dockerfile.claude"), RECIPE).unwrap();
    std::fs::write(custom.join("notes.md"), "user data").unwrap();
    let custom = std::fs::canonicalize(&custom).unwrap();
    let task = task("delta", &custom);
    let snapshot = tree(&custom);

    let run = prepare(&f, &task).await;
    let prepared = run.prepared.unwrap().unwrap();
    let context = f.paths.task_dir("delta").unwrap();
    assert_eq!(
        std::fs::read_to_string(context.join("Dockerfile.dev")).unwrap(),
        "FROM scratch\nCOPY scripts/setup.sh /setup.sh\n"
    );
    assert!(
        context.join("scripts/setup.sh").is_file(),
        "COPY input moved"
    );
    assert_eq!(
        std::fs::read_to_string(context.join(".awman/Dockerfile.claude")).unwrap(),
        RECIPE
    );
    assert!(!context.join("notes.md").exists());
    assert_eq!(
        tree(&custom),
        snapshot,
        "the custom folder is never written"
    );
    assert!(f
        .paths
        .tasks_dir()
        .join("delta/image-context.json")
        .is_file());
    // Tags are per task, not per folder name; builds use the task context.
    assert!(
        prepared.project.tag.contains("squad-delta"),
        "{}",
        prepared.project.tag
    );
    let canonical_context = std::fs::canonicalize(&context).unwrap();
    for argv in builds(&f.docker, 0) {
        assert_eq!(
            std::fs::canonicalize(argv.last().unwrap()).unwrap(),
            canonical_context
        );
        assert!(!argv.iter().any(|a| a.contains("checkout")), "{argv:?}");
    }

    // Edits in the custom folder no longer feed the images, and the
    // migration does not run again.
    std::fs::write(
        custom.join("Dockerfile.dev"),
        "FROM scratch\nRUN echo legacy\n",
    )
    .unwrap();
    let before = f.docker.calls().len();
    let again = prepare(&f, &task).await;
    again.prepared.unwrap().unwrap();
    assert!(builds(&f.docker, before).is_empty());
    assert!(!again.messages.iter().any(|m| m.contains("moved ")));
    f.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn unmovable_legacy_recipe_fails_before_any_build_and_changes_nothing() {
    let f = fixture("engine::image_prep::squad_image_inputs::unmovable_legacy_recipe_fails_before_any_build_and_changes_nothing");
    let custom = f.sandbox.work().join("whole");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::write(custom.join("Dockerfile.dev"), "FROM scratch\nCOPY . /src\n").unwrap();
    let custom = std::fs::canonicalize(&custom).unwrap();
    let task = task("epsilon", &custom);
    let run = prepare(&f, &task).await;
    let error = run.prepared.unwrap_err().to_string();
    assert!(error.contains("Nothing was changed"), "{error}");
    assert!(error.contains("copies the whole directory"), "{error}");
    let context = f.paths.task_dir("epsilon").unwrap();
    assert!(!context.join("Dockerfile.dev").exists());
    assert!(!f
        .paths
        .tasks_dir()
        .join("epsilon/image-context.json")
        .exists());
    assert!(builds(&f.docker, 0).is_empty());
    finished_once(&run.events);
    f.sandbox.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_preparations_build_the_base_once_and_agree_on_one_generation() {
    let f = fixture("engine::image_prep::squad_image_inputs::concurrent_preparations_build_the_base_once_and_agree_on_one_generation");
    let root = seed_default(&f, "zeta");
    let task = task("zeta", &root);
    f.docker
        .on(&["build"])
        .stdout("held\n")
        .exit(0)
        .hold_until_released();
    let f = Arc::new(f);
    let (a, b) = (f.clone(), f.clone());
    let (ta, tb) = (task.clone(), task.clone());
    let first = tokio::spawn(async move { prepare(&a, &ta).await.prepared.unwrap().unwrap() });
    tokio::task::spawn_blocking({
        let f = f.clone();
        move || f.docker.wait_until_held(Duration::from_secs(20))
    })
    .await
    .unwrap();
    let project = project_image_tag(&root);
    let before = inspections(&f, &project);
    let second = tokio::spawn(async move { prepare(&b, &tb).await.prepared.unwrap().unwrap() });
    // Release only once the second preparation has found the base missing
    // and is therefore heading for the held build's lease.
    tokio::task::spawn_blocking({
        let f = f.clone();
        move || {
            awman_test_support::wait_for(
                "second preparation to inspect the base",
                Duration::from_secs(20),
                || (inspections(&f, &project) > before).then_some(()),
            )
        }
    })
    .await
    .unwrap();
    f.docker.release();
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    let tags: Vec<String> = builds(&f.docker, 0).iter().map(|a| tag_of(a)).collect();
    assert_eq!(
        tags.iter().filter(|t| **t == first.project.tag).count(),
        1,
        "{tags:?}"
    );
    assert_eq!(first.project.image_id, second.project.image_id);
    assert_eq!(
        first.agents["claude"].image_id,
        second.agents["claude"].image_id
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_symlinked_recipe_is_refused_not_followed() {
    let f = fixture(
        "engine::image_prep::squad_image_inputs::a_symlinked_recipe_is_refused_not_followed",
    );
    let root = seed_default(&f, "eta");
    let outside = f.sandbox.work().join("outside.Dockerfile");
    std::fs::write(&outside, "FROM evil\n").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("Dockerfile.dev")).unwrap();
    let run = prepare(&f, &task("eta", &root)).await;
    assert!(run.prepared.is_err());
    assert!(builds(&f.docker, 0).is_empty());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "FROM evil\n");
    f.sandbox.finish();
}

fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// How many times the base image `project` was inspected so far.
fn inspections(f: &Fixture, project: &str) -> usize {
    f.docker
        .calls()
        .iter()
        .filter(|c| c.argv.starts_with(&["image".into(), "inspect".into()]))
        .filter(|c| c.argv.last().map(String::as_str) == Some(project))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_image_that_cannot_be_inspected_fails_preparation_instead_of_being_reused() {
    let f = fixture("engine::image_prep::squad_image_inputs::an_image_that_cannot_be_inspected_fails_preparation_instead_of_being_reused");
    let root = seed_default(&f, "delta");
    let task = task("delta", &root);
    prepare(&f, &task).await.prepared.unwrap().unwrap();
    // The runtime's metadata endpoint becomes unavailable: the stateful
    // store stops answering and inspection fails with a daemon error.
    std::fs::rename(
        f.docker.dir().join("images"),
        f.docker.dir().join("images.down"),
    )
    .unwrap();
    f.docker
        .on(&["image", "inspect"])
        .stderr("Cannot connect to the Docker daemon\n")
        .exit(1);
    let run = prepare(&f, &task).await;
    let error = run.prepared.unwrap_err().to_string();
    assert!(error.contains("could not verify image"), "{error}");
    assert!(run.session.lease_images().is_none(), "nothing is pinned");
    finished_once(&run.events);
    f.sandbox.finish();
}
