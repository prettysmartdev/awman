//! WI 0126 A1: one process-wide builtin runtime, many sessions.
//!
//! A TUI process clones one `Engines` into every tab, the API server shares
//! one across its sessions, and the squad daemon builds one from the global
//! config alone. Every launch must still be sized and confined by its own
//! session's repo config. These tests build one `Engines` over the real
//! `BuiltinBackend` (on an in-memory driver) and open sessions the way each
//! frontend does, from two different repos.

use std::path::Path;
use std::sync::Arc;

use super::Engines;
use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME, HOME};
use crate::data::ready_summary::ReadyImageAction;
use crate::data::session::{AgentName, Session, SessionOpenOptions};
use crate::data::session_manager::SessionManager;
use crate::engine::agent::AgentRunOptions;
use crate::engine::agent_runtime::DetectedRuntime;
use crate::engine::auth::AgentCredentials;
use crate::engine::container::background::BackgroundLaunchRequest;
use crate::engine::container::{FakeBuiltin, LaunchedVm};
use crate::engine::error::EngineError;

/// The global policy every test starts from: an allowlist the repos narrow.
const GLOBAL: &str =
    r#"{"vcpus":2,"memoryMib":4096,"network":{"mode":"allowlist","allow":["*.example.com"]}}"#;
/// Repo A narrows the allowlist to one host and runs smaller VMs.
const REPO_A: &str = r#"{"vcpus":1,"memoryMib":1024,"network":{"allow":["api.example.com"]}}"#;
/// Repo B turns the network off.
const REPO_B: &str = r#"{"vcpus":3,"network":{"mode":"none"}}"#;

/// A user's config home holding a global `builtin` block, and the env
/// snapshot every session in the process shares.
struct Host {
    home: tempfile::TempDir,
}

impl Host {
    fn new(global_builtin: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.json"),
            format!(r#"{{"builtin":{global_builtin}}}"#),
        )
        .unwrap();
        Self { home }
    }

    fn env(&self) -> EnvSnapshot {
        let home = self.home.path().to_str().unwrap();
        EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home), (HOME, home)])
    }

    fn options(&self) -> SessionOpenOptions {
        SessionOpenOptions {
            env: Some(self.env()),
            ..Default::default()
        }
    }

    /// The process's session manager, built (as the TUI and the API server
    /// build theirs) from the process environment.
    fn manager(&self) -> SessionManager {
        SessionManager::in_memory(self.env())
    }

    /// A TUI tab opened on `root`, exactly as `App::add_tab` opens one: through
    /// the process's `SessionManager` with default options, so the env comes
    /// from the manager and not from the test.
    fn tui_tab(&self, root: &Path) -> Session {
        let manager = self.manager();
        let id = manager
            .open_or_create(root.to_path_buf(), SessionOpenOptions::default())
            .unwrap();
        let session = manager.get(&id).unwrap();
        let session = session.try_read().unwrap().clone();
        session
    }

    /// An API session on `root`, as API session setup opens one
    /// (`open_or_create_with_key` with default options).
    fn api_session(&self, root: &Path) -> Session {
        let session = self
            .manager()
            .open_or_create_with_key(None, root.to_path_buf(), SessionOpenOptions::default())
            .unwrap();
        let session = session.try_read().unwrap().clone();
        session
    }

    /// A worktree session opened for `invoker`, as `exec workflow` and the
    /// dynamic leader open theirs.
    fn worktree(&self, checkout: &Path, invoker: &Session) -> Session {
        Session::open_worktree_for(checkout.to_path_buf(), checkout.to_path_buf(), invoker).unwrap()
    }

    /// A squad task's session (`SquadEvaluation::open_session` →
    /// `Session::open_at_git_root` with the daemon's env).
    fn squad_task(&self, root: &Path) -> Session {
        Session::open_at_git_root(root.to_path_buf(), root.to_path_buf(), self.options()).unwrap()
    }
}

/// A repository whose `.awman/config.json` holds `builtin` (or nothing).
fn repo(builtin: Option<&str>) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    if let Some(block) = builtin {
        let dir = root.path().join(".awman");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), format!(r#"{{"builtin":{block}}}"#)).unwrap();
    }
    root
}

/// The process's one `Engines`, built (as `Startup` builds it) from whichever
/// session the process started in.
fn engines(fake: &FakeBuiltin, first: &Session) -> Engines {
    Engines::from_detected(DetectedRuntime::Container(fake.runtime.clone()), first).unwrap()
}

fn claude() -> AgentName {
    AgentName::new("claude").unwrap()
}

/// Launch the session's agent in the foreground (through `plan()`).
async fn launch(fake: &FakeBuiltin, engines: &Engines, session: &Session) -> LaunchedVm {
    fake.import(&crate::data::image_tags::agent_image_tag(
        session.git_root(),
        "claude",
    ));
    let options = engines
        .agent_engine
        .resolve_agent_options(
            session,
            &claude(),
            &AgentRunOptions::default(),
            &AgentCredentials::default(),
        )
        .unwrap();
    let instance = engines.runtime.build(options).unwrap();
    let (id, _execution) = fake.run(instance).await;
    fake.launched(&id)
}

/// Start a setup/teardown container for the session (through
/// `start_background()`), with the launch settings Layer 2 passes.
fn launch_background(fake: &FakeBuiltin, engines: &Engines, session: &Session) -> LaunchedVm {
    let image = "awman-base:latest";
    fake.import(image);
    let launch = engines
        .agent_engine
        .builtin_launch_settings(session)
        .unwrap();
    let env = std::collections::HashMap::new();
    let container = fake
        .runtime
        .launch_background(&BackgroundLaunchRequest {
            secret_env: &[],
            name: None,
            image,
            workdir: session.git_root(),
            env: &env,
            overlays: &[],
            builtin_launch: launch.as_ref(),
        })
        .unwrap();
    fake.launched(container.container_id())
}

/// What a session's launches must look like: its own resolved settings.
fn expected(session: &Session) -> (u8, u32, serde_json::Value) {
    let settings = session
        .effective_config()
        .builtin_launch_settings()
        .unwrap();
    (
        settings.vcpus,
        settings.memory_mib,
        FakeBuiltin::policy_for(&settings.network),
    )
}

fn shape(vm: &LaunchedVm) -> (u8, u32, serde_json::Value) {
    (vm.vcpus, vm.memory_mib, vm.network_policy.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn two_repos_sharing_one_engines_each_launch_with_their_own_settings() {
    let host = Host::new(GLOBAL);
    let (repo_a, repo_b) = (repo(Some(REPO_A)), repo(Some(REPO_B)));
    let fake = FakeBuiltin::new();
    // A TUI process started in repo A; repo B is a second tab. An API server
    // sharing its `Engines` across sessions is the same shape.
    let tab_a = host.tui_tab(repo_a.path());
    let tab_b = host.tui_tab(repo_b.path());
    let api_b = host.api_session(repo_b.path());
    let engines = engines(&fake, &tab_a);

    for (session, vcpus, memory, enabled) in [
        (&tab_a, 1, 1024, true),
        (&tab_b, 3, 4096, false),
        (&api_b, 3, 4096, false),
    ] {
        for vm in [
            launch(&fake, &engines, session).await,
            launch_background(&fake, &engines, session),
        ] {
            assert_eq!(shape(&vm), expected(session));
            assert_eq!((vm.vcpus, vm.memory_mib), (vcpus, memory));
            assert_eq!(vm.network_enabled, enabled);
        }
    }
    let a = launch(&fake, &engines, &tab_a).await;
    let b = launch(&fake, &engines, &tab_b).await;
    assert_ne!(
        a.network_policy, b.network_policy,
        "repo A's narrowing must not leak into repo B's tab or the reverse"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_without_a_builtin_block_uses_the_global_policy() {
    let host = Host::new(GLOBAL);
    let (narrowing, plain) = (repo(Some(REPO_A)), repo(None));
    let fake = FakeBuiltin::new();
    let first = host.tui_tab(narrowing.path());
    let engines = engines(&fake, &first);
    let tab = host.tui_tab(plain.path());
    let vm = launch(&fake, &engines, &tab).await;
    assert_eq!((vm.vcpus, vm.memory_mib), (2, 4096));
    assert_eq!(shape(&vm), expected(&tab));
    assert_ne!(
        vm.network_policy,
        launch(&fake, &engines, &first).await.network_policy
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_squad_task_inherits_its_repos_narrowing() {
    // The squad daemon's `Engines` knows only the global config
    // (`Engines::for_daemon`); the task's own session supplies the rest.
    let host = Host::new(GLOBAL);
    let daemon_root = repo(None);
    let task_repo = repo(Some(REPO_A));
    let fake = FakeBuiltin::new();
    let engines = engines(&fake, &host.squad_task(daemon_root.path()));

    let task = host.squad_task(task_repo.path());
    for vm in [
        launch(&fake, &engines, &task).await,
        launch_background(&fake, &engines, &task),
    ] {
        assert_eq!((vm.vcpus, vm.memory_mib), (1, 1024));
        assert_eq!(shape(&vm), expected(&task));
    }
    let global_only = launch(&fake, &engines, &host.squad_task(daemon_root.path())).await;
    assert_ne!(
        global_only.network_policy,
        launch(&fake, &engines, &task).await.network_policy,
        "the task's narrowing applies, not the daemon's global policy"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_widening_repo_fails_its_own_launches_and_no_one_elses() {
    let host = Host::new(r#"{"network":{"mode":"none"}}"#);
    let (widening, fine) = (repo(Some(r#"{"network":{"mode":"public"}}"#)), repo(None));
    let fake = FakeBuiltin::new();
    let good = host.tui_tab(fine.path());
    let engines = engines(&fake, &good);
    let bad = host.api_session(widening.path());

    let error = engines
        .agent_engine
        .resolve_agent_options(
            &bad,
            &claude(),
            &AgentRunOptions::default(),
            &AgentCredentials::default(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("builtin.network"), "{error}");
    assert!(engines.agent_engine.builtin_launch_settings(&bad).is_err());
    assert_eq!(fake.creates(), 0);

    let vm = launch(&fake, &engines, &good).await;
    assert!(!vm.network_enabled, "the other tab keeps the global policy");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_state_dir_must_match_the_process_one() {
    let host = Host::new("{}");
    let fake = FakeBuiltin::new();
    let same = repo(Some(&format!(
        r#"{{"stateDir":{}}}"#,
        serde_json::to_string(&fake.state_dir).unwrap()
    )));
    let other = repo(Some(r#"{"stateDir":"/somewhere/else"}"#));
    let first = host.tui_tab(same.path());
    let engines = engines(&fake, &first);

    // Equal: accepted.
    launch(&fake, &engines, &first).await;

    // Different: that launch fails with both paths; the other tab still works.
    let foreign = host.tui_tab(other.path());
    fake.import(&crate::data::image_tags::agent_image_tag(
        foreign.git_root(),
        "claude",
    ));
    let options = engines
        .agent_engine
        .resolve_agent_options(
            &foreign,
            &claude(),
            &AgentRunOptions::default(),
            &AgentCredentials::default(),
        )
        .unwrap();
    match engines.runtime.build(options).err().unwrap() {
        EngineError::BuiltinStateDirMismatch { process, session } => {
            assert_eq!(process, fake.state_dir);
            assert_eq!(session, std::path::PathBuf::from("/somewhere/else"));
        }
        other => panic!("expected BuiltinStateDirMismatch, got {other:?}"),
    }
    let launch_settings = engines
        .agent_engine
        .builtin_launch_settings(&foreign)
        .unwrap();
    let env = std::collections::HashMap::new();
    assert!(matches!(
        fake.runtime.launch_background(&BackgroundLaunchRequest {
            secret_env: &[],
            name: None,
            image: "awman-base:latest",
            workdir: foreign.git_root(),
            env: &env,
            overlays: &[],
            builtin_launch: launch_settings.as_ref(),
        }),
        Err(EngineError::BuiltinStateDirMismatch { .. })
    ));
    launch(&fake, &engines, &first).await;
}

/// Lifted from `ReadyEngine`'s
/// `two_sessions_with_opposite_builder_settings_each_see_only_their_own`: a
/// TUI tab and an API session sharing one `Engines` each get the builder
/// decision of their own repo, constructed the way `ReadyCommand` and API
/// session setup construct their `ReadyEngine`.
#[test]
fn a_tui_tab_and_an_api_session_each_honour_their_own_builder_flag() {
    let host = Host::new(r#"{"builder":{"enabled":true}}"#);
    let (enabled, disabled) = (repo(None), repo(Some(r#"{"builder":{"enabled":false}}"#)));
    let fake = FakeBuiltin::new();
    let tab = host.tui_tab(enabled.path());
    let api = host.api_session(disabled.path());
    let engines = engines(&fake, &tab);
    let ready = |session: &Session| {
        crate::engine::ready::ReadyEngine::new(
            Some(session.effective_config().builtin_settings().unwrap()),
            crate::data::config::global::GlobalConfig::data_home_with(
                session.effective_config().env(),
            )
            .unwrap(),
            Arc::new(session.clone()),
            engines.git_engine.clone(),
            engines.overlay_engine.clone(),
            engines.require_container_runtime().unwrap().clone(),
            engines.agent_engine.clone(),
            crate::engine::ready::ReadyEngineOptions {
                agent: claude(),
                refresh: false,
                build: false,
                no_cache: false,
                allow_docker: false,
                non_interactive: true,
                env_passthrough: None,
            },
        )
        .summary()
        .image_action
    };
    assert_eq!(ready(&tab), ReadyImageAction::BuildAndImport);
    assert_eq!(ready(&api), ReadyImageAction::Import);
    // Same two repos, opposite frontends: the repo decides, not the frontend
    // or the session the process started with.
    assert_eq!(
        ready(&host.api_session(enabled.path())),
        ReadyImageAction::BuildAndImport
    );
    assert_eq!(
        ready(&host.tui_tab(disabled.path())),
        ReadyImageAction::Import
    );
}

/// A resolver that finds no git repository, as in a plain directory.
struct NoGit;

impl crate::data::session::GitRootResolver for NoGit {
    fn resolve(
        &self,
        working_dir: &Path,
    ) -> Result<std::path::PathBuf, crate::data::error::DataError> {
        Err(crate::data::error::DataError::GitRootNotFound {
            working_dir: working_dir.to_path_buf(),
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_git_directory_session_uses_the_global_policy() {
    // The workdir-fallback branch (`is_git_repo = false`), not the
    // successful-resolver one the other tests take.
    let host = Host::new(GLOBAL);
    let plain = repo(None);
    let fake = FakeBuiltin::new();
    let engines = engines(&fake, &host.tui_tab(plain.path()));
    let session =
        Session::open_or_workdir_fallback(plain.path().to_path_buf(), &NoGit, host.options())
            .unwrap();
    assert!(!session.is_git_repo());
    let vm = launch(&fake, &engines, &session).await;
    assert_eq!((vm.vcpus, vm.memory_mib), (2, 4096));
    assert_eq!(shape(&vm), expected(&session));
    // A local `.awman/config.json` in a non-git directory is still that
    // session's repo config (the fallback loads it), so it narrows too.
    let local = repo(Some(REPO_A));
    let session =
        Session::open_or_workdir_fallback(local.path().to_path_buf(), &NoGit, host.options())
            .unwrap();
    let vm = launch(&fake, &engines, &session).await;
    assert_eq!((vm.vcpus, vm.memory_mib), (1, 1024));
}

/// C1 closes A1's recording-builder handoff: one Engines, both frontend
/// session shapes, actual preparation under Ready and SessionSetup policies.
#[tokio::test]
async fn shared_engines_preparation_obeys_each_sessions_builder_flag() {
    use crate::data::step_status::StepStatus;
    use crate::engine::image_prep::{
        AgentImagePreparer, AgentImageRequest, BuildPolicy, ImageBuildContext, ImageImporter,
        MessageSink,
    };
    use crate::engine::ready::tests::fakes::{FakeImportRuntime, FakeLocalBuilder};
    use crate::engine::ready::tests::{built_identity, script_missing_image_build};
    struct Importer(Arc<FakeImportRuntime>);
    impl ImageImporter for Importer {
        fn import(
            &self,
            request: &crate::engine::agent_runtime::ImageImportRequest,
            cancel: &crate::engine::oci::CancelToken,
            sink: &mut dyn crate::data::message::UserMessageSink,
        ) -> Result<crate::engine::agent_runtime::ImportedImage, EngineError> {
            crate::engine::agent_runtime::AgentRuntimeEngine::import_image_cancellable(
                self.0.as_ref(),
                request,
                sink,
                cancel,
            )
        }
    }
    let host = Host::new(
        r#"{"builder":{"enabled":true},"imageSource":{"type":"docker-local","host":"unix:///test/docker.sock"}}"#,
    );
    let enabled = repo(None);
    let disabled = repo(Some(r#"{"builder":{"enabled":false}}"#));
    for root in [enabled.path(), disabled.path()] {
        std::fs::create_dir_all(root.join(".awman")).unwrap();
        std::fs::write(root.join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        std::fs::write(root.join(".awman/Dockerfile.claude"), "FROM scratch\n").unwrap();
    }
    let fake = FakeBuiltin::new();
    let tab = host.tui_tab(enabled.path());
    let engines = engines(&fake, &tab);
    let builder = Arc::new(FakeLocalBuilder::new());
    let imported = Arc::new(FakeImportRuntime::new(false));
    for (session, policy, enabled) in [
        (
            tab,
            BuildPolicy::Ready {
                build: false,
                no_cache: false,
                refresh: false,
            },
            true,
        ),
        (
            host.api_session(disabled.path()),
            BuildPolicy::SessionSetup,
            false,
        ),
        (
            host.api_session(enabled.path()),
            BuildPolicy::SessionSetup,
            true,
        ),
        (
            host.tui_tab(disabled.path()),
            BuildPolicy::Ready {
                build: false,
                no_cache: false,
                refresh: false,
            },
            false,
        ),
    ] {
        let tag = crate::data::image_tags::agent_image_tag(session.git_root(), "claude");
        let base = crate::data::image_tags::project_image_tag(session.git_root());
        if enabled {
            script_missing_image_build(
                &builder,
                &base,
                &tag,
                built_identity('a', crate::data::oci_identity::OciPlatform::host_linux()),
                built_identity('b', crate::data::oci_identity::OciPlatform::host_linux()),
            );
        }
        let prior_builds = builder.build_calls().len();
        let prior_inspects = builder.inspect_count();
        let mut preparer = AgentImagePreparer::new(
            session.effective_config().builtin_settings().unwrap(),
            engines.runtime.clone(),
            host.home.path().join("data"),
            ImageBuildContext::for_session(&session, "claude"),
        )
        .with_local_builder(builder.clone())
        .with_image_importer(Arc::new(Importer(imported.clone())));
        let mut messages = crate::data::message::RecordingMessageSink::default();
        let result = preparer
            .prepare(
                &AgentImageRequest {
                    tag: &tag,
                    agent: "claude",
                    policy,
                },
                &mut MessageSink(&mut messages),
            )
            .await
            .unwrap();
        assert_eq!(result.import, StepStatus::Done);
        assert_eq!(
            result.build,
            if enabled {
                StepStatus::Done
            } else {
                StepStatus::Skipped
            }
        );
        assert_eq!(
            builder.build_calls().len() - prior_builds,
            if enabled { 2 } else { 0 }
        );
        if !enabled {
            assert_eq!(builder.inspect_count(), prior_inspects);
        }
    }
    assert_eq!(imported.imports.lock().unwrap().len(), 4);
}

/// A TUI tab or API session opened with default options gets the process's
/// environment from its `SessionManager`, so a global network policy under a
/// relocated `AWMAN_CONFIG_HOME` still confines it. An empty env would read
/// `~/.awman` instead and fall back to the `public` default.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_opened_with_default_options_keep_a_relocated_global_policy() {
    let host = Host::new(r#"{"network":{"mode":"none"}}"#);
    let plain = repo(None);
    let fake = FakeBuiltin::new();
    let tab = host.tui_tab(plain.path());
    let engines = engines(&fake, &tab);
    for session in [tab, host.api_session(plain.path())] {
        assert_eq!(
            session.env().get(AWMAN_CONFIG_HOME),
            host.env().get(AWMAN_CONFIG_HOME)
        );
        let vm = launch(&fake, &engines, &session).await;
        assert!(
            !vm.network_enabled,
            "the global `none` policy must survive a default-options open"
        );
    }

    // What the regression looked like: an empty env misses the relocated
    // global file entirely.
    let empty = Session::open_at_git_root(
        plain.path().to_path_buf(),
        plain.path().to_path_buf(),
        SessionOpenOptions {
            env: Some(EnvSnapshot::empty()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(empty.global_config().builtin.is_none());
}

/// A worktree checkout is cut from the last commit, so its config can lack
/// the invoker's uncommitted `builtin` block. The worktree session keeps the
/// invoker's narrowing (and default agent), not the checkout's.
#[tokio::test(flavor = "multi_thread")]
async fn a_worktree_session_keeps_the_invokers_uncommitted_narrowing() {
    let host = Host::new(GLOBAL);
    let invoking = repo(Some(REPO_B));
    std::fs::write(
        invoking.path().join(".awman/config.json"),
        format!(r#"{{"agent":"codex","builtin":{REPO_B}}}"#),
    )
    .unwrap();
    // The checkout's committed config: no builtin block, a different agent.
    let checkout = repo(None);
    std::fs::create_dir_all(checkout.path().join(".awman")).unwrap();
    std::fs::write(
        checkout.path().join(".awman/config.json"),
        r#"{"agent":"claude"}"#,
    )
    .unwrap();
    let fake = FakeBuiltin::new();
    let invoker = host.tui_tab(invoking.path());
    let engines = engines(&fake, &invoker);
    let worktree = host.worktree(checkout.path(), &invoker);

    assert_eq!(worktree.git_root(), checkout.path());
    assert_eq!(worktree.default_agent().map(|a| a.as_str()), Some("codex"));
    assert_eq!(
        worktree.effective_config().agent().as_deref(),
        Some("codex")
    );
    assert_eq!(expected(&worktree), expected(&invoker));
    for vm in [
        launch(&fake, &engines, &worktree).await,
        launch_background(&fake, &engines, &worktree),
    ] {
        assert!(!vm.network_enabled, "repo B's `none` must carry over");
        assert_eq!(vm.vcpus, 3);
    }
}
