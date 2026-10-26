use crate::data::oci_identity::BuiltImageIdentity;

use std::sync::Arc;

use super::*;
pub(crate) mod fakes;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::session::{SessionOpenOptions, StaticGitRootResolver};
use crate::data::step_status::StepStatus;
use crate::engine::error::EngineError;
use crate::engine::overlay::OverlayEngine;
use fakes::*;

// ── Fake frontend ────────────────────────────────────────────────────────

fn make_engine_and_frontend(
    create_dockerfile: bool,
    run_audit: bool,
) -> (ReadyEngine, FakeReadyFrontend, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    // Pre-create .awman/Dockerfile.claude so the ready engine does not
    // attempt a network download during tests.
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
    let resolver = StaticGitRootResolver::new(tmp.path());
    let session = Arc::new(
        crate::data::session::Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap(),
    );
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
    ));
    let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
        Arc::new(crate::engine::container::ContainerRuntime::docker());
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay.clone(),
        runtime.clone(),
    ));
    let options = ReadyEngineOptions {
        agent: AgentName::new("claude").unwrap(),
        refresh: false,
        build: true,
        no_cache: false,
        allow_docker: false,
        non_interactive: false,
        env_passthrough: None,
    };
    let engine = ReadyEngine::new(
        ReadyEngine::builtin_inputs(&session, runtime.as_ref()).unwrap(),
        crate::data::config::global::GlobalConfig::data_home_with(session.effective_config().env())
            .unwrap(),
        session,
        Arc::new(GitEngine::new()),
        overlay,
        runtime,
        agent_engine,
        options,
    );
    let frontend = FakeReadyFrontend {
        create_dockerfile,
        run_audit,
        phases: Vec::new(),
        statuses: Vec::new(),
        last_dockerfile_prompt_path: None,
        messages: Vec::new(),
    };
    (engine, frontend, tmp)
}

// ── Tests ────────────────────────────────────────────────────────────────
//
// The host-ping tests moved to `host_agent.rs` with the code they test.

#[tokio::test]
async fn awaiting_dockerfile_decision_false_leads_to_failed_phase() {
    let (mut engine, mut frontend, _tmp) = make_engine_and_frontend(false, true);
    let summary = engine.run_to_completion(&mut frontend).await.unwrap();
    assert!(
        matches!(engine.phase(), ReadyPhase::Failed(_)),
        "expected Failed phase, got {:?}",
        engine.phase()
    );
    // Summary fields should still be Pending (nothing ran after abort).
    assert!(matches!(summary.base_image, StepStatus::Pending));
    assert!(!summary.is_ready());
}

#[tokio::test]
async fn each_phase_reachable_via_step_calls() {
    let (mut engine, mut frontend, _tmp) = make_engine_and_frontend(true, false);
    // Step through from Preflight to Awaiting* phases individually.
    assert_eq!(engine.phase(), &ReadyPhase::Preflight);
    engine.step(&mut frontend).await.unwrap();
    assert_eq!(engine.phase(), &ReadyPhase::AwaitingDockerfileDecision);
    engine.step(&mut frontend).await.unwrap();
    assert_eq!(engine.phase(), &ReadyPhase::CreatingDockerfile);
}

#[tokio::test]
async fn creating_dockerfile_phase_writes_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (_engine, mut frontend, _tmp2) = make_engine_and_frontend(true, false);
    // We want to test the CreatingDockerfile phase specifically. Use a
    // dedicated tmpdir so we can check file creation.
    let resolver = crate::data::session::StaticGitRootResolver::new(tmp.path());
    let session = Arc::new(
        crate::data::session::Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            crate::data::session::SessionOpenOptions::default(),
        )
        .unwrap(),
    );
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
    ));
    let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
        Arc::new(crate::engine::container::ContainerRuntime::docker());
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay.clone(),
        runtime.clone(),
    ));
    let options = ReadyEngineOptions {
        agent: AgentName::new("claude").unwrap(),
        refresh: false,
        build: false,
        no_cache: false,
        allow_docker: false,
        non_interactive: false,
        env_passthrough: None,
    };
    let mut engine2 = ReadyEngine::new(
        ReadyEngine::builtin_inputs(&session, runtime.as_ref()).unwrap(),
        crate::data::config::global::GlobalConfig::data_home_with(session.effective_config().env())
            .unwrap(),
        session,
        Arc::new(GitEngine::new()),
        overlay,
        runtime,
        agent_engine,
        options,
    );
    // Step to AwaitingDockerfileDecision, then accept to move to CreatingDockerfile.
    engine2.step(&mut frontend).await.unwrap(); // Preflight → AwaitingDockerfileDecision
    engine2.step(&mut frontend).await.unwrap(); // AwaitingDockerfileDecision → CreatingDockerfile
                                                // Execute CreatingDockerfile phase.
    engine2.step(&mut frontend).await.unwrap(); // CreatingDockerfile → BuildingBaseImage
    let dockerfile = tmp.path().join("Dockerfile.dev");
    assert!(
        dockerfile.exists(),
        "CreatingDockerfile phase must write Dockerfile.dev to git root"
    );
    let content = std::fs::read_to_string(&dockerfile).unwrap();
    assert!(
        !content.is_empty(),
        "Dockerfile.dev must contain the template content"
    );
}

// ─── WI-0086: configured dockerfile path in ready ─────────────────────────

/// Build a ReadyEngine rooted at `git_root` with the agent Dockerfile
/// pre-created so no network downloads happen during tests. Mirrors
/// `make_engine_and_frontend` but lets callers stage their own
/// `.awman/config.json` before the Session is opened (so RepoConfig is
/// read from disk with the test's contents).
fn make_engine_at(git_root: &std::path::Path) -> ReadyEngine {
    let awman_dir = git_root.join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
    let resolver = StaticGitRootResolver::new(git_root);
    let session = Arc::new(
        crate::data::session::Session::open(
            git_root.to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap(),
    );
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(git_root),
    ));
    let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> =
        Arc::new(crate::engine::container::ContainerRuntime::docker());
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay.clone(),
        runtime.clone(),
    ));
    let options = ReadyEngineOptions {
        agent: AgentName::new("claude").unwrap(),
        refresh: false,
        build: false,
        no_cache: false,
        allow_docker: false,
        non_interactive: false,
        env_passthrough: None,
    };
    ReadyEngine::new(
        ReadyEngine::builtin_inputs(&session, runtime.as_ref()).unwrap(),
        crate::data::config::global::GlobalConfig::data_home_with(session.effective_config().env())
            .unwrap(),
        session,
        Arc::new(GitEngine::new()),
        overlay,
        runtime,
        agent_engine,
        options,
    )
}

#[tokio::test]
async fn ready_with_configured_dockerfile_reports_configured_path_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    // Configure a non-default dockerfile path, but do not create the file.
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(
        awman_dir.join("config.json"),
        r#"{"dockerfile":"infra/Dockerfile.base"}"#,
    )
    .unwrap();

    let mut engine = make_engine_at(tmp.path());
    let mut frontend = FakeReadyFrontend {
        create_dockerfile: false, // decline so the engine fails fast
        run_audit: false,
        phases: Vec::new(),
        statuses: Vec::new(),
        last_dockerfile_prompt_path: None,
        messages: Vec::new(),
    };

    engine.step(&mut frontend).await.unwrap(); // Preflight
    engine.step(&mut frontend).await.unwrap(); // AwaitingDockerfileDecision

    let prompted = frontend
        .last_dockerfile_prompt_path
        .as_ref()
        .expect("ask_create_dockerfile must be called when the configured path is missing");
    assert_eq!(
        prompted,
        &tmp.path().join("infra/Dockerfile.base"),
        "ready must surface the configured dockerfile path, not the default Dockerfile.dev"
    );
}

#[tokio::test]
async fn ready_with_configured_dockerfile_skips_decision_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(
        awman_dir.join("config.json"),
        r#"{"dockerfile":"infra/Dockerfile.base"}"#,
    )
    .unwrap();
    // Create the configured Dockerfile so Preflight short-circuits past
    // AwaitingDockerfileDecision.
    let infra_dir = tmp.path().join("infra");
    std::fs::create_dir_all(&infra_dir).unwrap();
    std::fs::write(infra_dir.join("Dockerfile.base"), "FROM scratch\n").unwrap();

    let mut engine = make_engine_at(tmp.path());
    let mut frontend = FakeReadyFrontend {
        create_dockerfile: false, // sentinel — if the prompt fires the engine would fail
        run_audit: false,
        phases: Vec::new(),
        statuses: Vec::new(),
        last_dockerfile_prompt_path: None,
        messages: Vec::new(),
    };

    engine.step(&mut frontend).await.unwrap(); // Preflight → BuildingBaseImage

    assert!(
        frontend.last_dockerfile_prompt_path.is_none(),
        "ready must not prompt for Dockerfile creation when the configured path exists; \
             got: {:?}",
        frontend.last_dockerfile_prompt_path
    );
    assert!(
        matches!(engine.summary().dockerfile, StepStatus::Done),
        "dockerfile step must be Done when the configured path exists; got: {:?}",
        engine.summary().dockerfile
    );
    assert_eq!(
        engine.phase(),
        &ReadyPhase::BuildingBaseImage,
        "engine must advance directly to BuildingBaseImage after Preflight"
    );
}

// ── Kit-declarative (sandbox-tier) ready flow (F-40b) ────────────────
//
// `ReadyEngine` is runtime-agnostic: given a kit-declarative runtime it
// skips the whole container phase machine in one step. Before F-40b this
// branch lived in Layer 2 (`commands/ready.rs`) and keyed on
// `engines.sandbox_runtime.is_some()`.

/// A kit-declarative runtime that records the `ready_agent` call and
/// refuses every image-store operation, the way the sandbox tier does.
#[tokio::test]
async fn ready_on_a_kit_declarative_runtime_applies_the_kit_and_skips_image_phases() {
    let tmp = tempfile::tempdir().unwrap();
    let resolver = StaticGitRootResolver::new(tmp.path());
    let session = Arc::new(
        crate::data::session::Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap(),
    );
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
    ));
    let kit = Arc::new(FakeKitRuntime::new());
    let runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = kit.clone();
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay.clone(),
        runtime.clone(),
    ));
    let mut engine = ReadyEngine::new(
        ReadyEngine::builtin_inputs(&session, runtime.as_ref()).unwrap(),
        crate::data::config::global::GlobalConfig::data_home_with(session.effective_config().env())
            .unwrap(),
        session,
        Arc::new(GitEngine::new()),
        overlay,
        runtime,
        agent_engine,
        ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build: true,
            no_cache: true,
            allow_docker: false,
            non_interactive: true,
            env_passthrough: None,
        },
    );
    let mut frontend = FakeReadyFrontend {
        create_dockerfile: false,
        run_audit: false,
        phases: Vec::new(),
        statuses: Vec::new(),
        last_dockerfile_prompt_path: None,
        messages: Vec::new(),
    };

    let phase = engine.step(&mut frontend).await.unwrap();

    assert_eq!(
        phase,
        ReadyPhase::Complete,
        "one step, straight to Complete"
    );
    let calls = kit.ready_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "the kit is applied exactly once");
    assert_eq!(calls[0].0, "claude");
    assert!(calls[0].1.no_cache, "no_cache must reach the runtime");

    let summary = engine.summary();
    assert_eq!(summary.agent_image, StepStatus::Done);
    // Every container-tier step is not applicable, and says so rather
    // than reporting a Done it never did.
    assert_eq!(summary.dockerfile, StepStatus::Skipped);
    assert_eq!(summary.base_image, StepStatus::Skipped);
    assert_eq!(summary.local_agent, StepStatus::Skipped);
    assert_eq!(summary.audit, StepStatus::Skipped);
}

// ── Import-acquisition (builtin) ready flow ──────────────────────────
//
// An importing runtime prepares an image that was built elsewhere. It is
// never asked to build, and a missing source explains the external build.

pub(crate) fn import_engine(
    runtime: Arc<FakeImportRuntime>,
    build: bool,
    sources: ImageSources,
) -> (ReadyEngine, FakeReadyFrontend, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    let resolver = StaticGitRootResolver::new(tmp.path());
    let session = Arc::new(
        crate::data::session::Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap(),
    );
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
    ));
    let dyn_runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = runtime;
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay.clone(),
        dyn_runtime.clone(),
    ));
    let engine = ReadyEngine::new(
        ReadyEngine::builtin_inputs(&session, dyn_runtime.as_ref()).unwrap(),
        crate::data::config::global::GlobalConfig::data_home_with(session.effective_config().env())
            .unwrap(),
        session,
        Arc::new(GitEngine::new()),
        overlay,
        dyn_runtime,
        agent_engine,
        ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build,
            no_cache: false,
            allow_docker: false,
            non_interactive: true,
            env_passthrough: None,
        },
    )
    .with_image_sources(sources);
    let frontend = FakeReadyFrontend {
        create_dockerfile: false,
        run_audit: false,
        phases: Vec::new(),
        statuses: Vec::new(),
        last_dockerfile_prompt_path: None,
        messages: Vec::new(),
    };
    (engine, frontend, tmp)
}

/// Step until the image phases are done (stops before the host ping).
async fn step_through_image_phases(engine: &mut ReadyEngine, frontend: &mut FakeReadyFrontend) {
    while !matches!(
        engine.phase(),
        ReadyPhase::CheckingNonDefaultAgents | ReadyPhase::Complete | ReadyPhase::Failed(_)
    ) {
        engine.step(frontend).await.unwrap();
    }
}

pub(crate) fn archive_sources() -> ImageSources {
    ImageSources {
        default: Some(
            crate::data::config::image_source::ImageSourceSpec::Archive {
                path: "/exports/agent.tar".into(),
            },
        ),
        images: Default::default(),
        registries: Default::default(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ready_import_timeout_cancels_worker_on_a_current_thread_runtime() {
    let mut fake = FakeImportRuntime::new(false);
    fake.wait_for_cancel = true;
    let runtime = Arc::new(fake);
    let (mut engine, mut frontend, tmp) = import_engine(runtime.clone(), true, archive_sources());
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        engine.import_default_agent_image(&mut frontend, tmp.path()),
    )
    .await;
    assert!(
        result.is_err(),
        "blocking import must not prevent the async timeout"
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !runtime
            .cancel_observed
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("dropped ready future must cancel its production worker");
    assert_eq!(runtime.imports.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn ready_caller_token_cancels_an_in_flight_import() {
    let mut fake = FakeImportRuntime::new(false);
    fake.wait_for_cancel = true;
    let runtime = Arc::new(fake);
    let (mut engine, mut frontend, tmp) = import_engine(runtime.clone(), true, archive_sources());
    let cancel = engine.cancel_token();
    let signal = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
    });
    engine
        .import_default_agent_image(&mut frontend, tmp.path())
        .await;
    signal.await.unwrap();
    assert!(runtime
        .cancel_observed
        .load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        matches!(engine.summary().agent_image, StepStatus::Failed(message) if message.contains("cancelled"))
    );
}

#[tokio::test]
async fn import_runtime_preserves_the_concrete_availability_error() {
    let mut fake = FakeImportRuntime::new(true);
    fake.unavailable = Some("cannot access /dev/kvm: Permission denied");
    let runtime = Arc::new(fake);
    let (mut engine, mut frontend, tmp) = import_engine(runtime.clone(), false, archive_sources());
    engine
        .import_default_agent_image(&mut frontend, tmp.path())
        .await;
    assert!(
        matches!(engine.summary().agent_image, StepStatus::Failed(message) if message.contains("/dev/kvm: Permission denied"))
    );
    assert!(runtime.imports.lock().unwrap().is_empty());
    assert_eq!(*runtime.builds.lock().unwrap(), 0);
}

/// WI 0126 A1: a repo-level `builtin.registries` block is the one value
/// both source resolution and the backend's pull use. The engine is built
/// from the session alone (no `with_image_sources`), as Layer 2 builds it.
#[tokio::test]
async fn a_repo_registries_block_drives_both_resolution_and_the_pull() {
    let home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    std::fs::create_dir_all(tmp.path().join(".awman")).unwrap();
    std::fs::write(
        tmp.path().join(".awman").join("config.json"),
        r#"{"builtin":{
                "imageSource":{"type":"archive","path":"/exports/agent.tar"},
                "registries":{"registry.example:5000":{"insecure":true}}
            }}"#,
    )
    .unwrap();
    let session = Arc::new(crate::data::session::Session::for_tests_isolated(
        tmp.path(),
        home.path(),
    ));
    let expected = session
        .effective_config()
        .builtin_settings()
        .unwrap()
        .registries;
    assert_eq!(expected.len(), 1);

    let runtime = Arc::new(FakeImportRuntime::new(false));
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(home.path()),
    ));
    let dyn_runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = runtime.clone();
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay.clone(),
        dyn_runtime.clone(),
    ));
    let mut engine = ReadyEngine::new(
        ReadyEngine::builtin_inputs(&session, dyn_runtime.as_ref()).unwrap(),
        crate::data::config::global::GlobalConfig::data_home_with(session.effective_config().env())
            .unwrap(),
        session,
        Arc::new(GitEngine::new()),
        overlay,
        dyn_runtime,
        agent_engine,
        ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build: false,
            no_cache: false,
            allow_docker: false,
            non_interactive: true,
            env_passthrough: None,
        },
    );
    assert_eq!(
        engine.preparer.image_sources().registries,
        expected,
        "source resolution reads the session's registries"
    );
    let mut frontend = FakeReadyFrontend {
        create_dockerfile: false,
        run_audit: false,
        phases: Vec::new(),
        statuses: Vec::new(),
        last_dockerfile_prompt_path: None,
        messages: Vec::new(),
    };
    engine
        .import_default_agent_image(&mut frontend, tmp.path())
        .await;
    let imports = runtime.imports.lock().unwrap().clone();
    assert!(!imports.is_empty());
    for request in imports {
        assert_eq!(
            request.registries, expected,
            "the pull uses the same session registries"
        );
    }
}

#[tokio::test]
async fn import_runtime_imports_the_agent_image_and_never_builds() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut engine, mut frontend, tmp) = import_engine(runtime.clone(), false, archive_sources());
    step_through_image_phases(&mut engine, &mut frontend).await;

    assert!(frontend.phases.contains(&ReadyPhase::ImportingBaseImage));
    assert!(frontend.phases.contains(&ReadyPhase::ImportingAgentImage));
    assert!(!frontend.phases.contains(&ReadyPhase::BuildingBaseImage));
    assert_eq!(
        *runtime.builds.lock().unwrap(),
        0,
        "an importing runtime never builds"
    );
    let imports = runtime.imports.lock().unwrap().clone();
    assert_eq!(imports.len(), 1);
    assert_eq!(
        imports[0].tag,
        crate::data::image_tags::agent_image_tag(tmp.path(), "claude")
    );
    assert!(!imports[0].refresh);
    assert_eq!(
        imports[0].platform,
        crate::data::oci_identity::OciPlatform::host_linux()
    );
    let summary = engine.summary();
    assert_eq!(summary.agent_image, StepStatus::Done);
    assert_eq!(summary.image_source, Some(StepStatus::Done));
    assert_eq!(summary.base_image, StepStatus::Skipped);
}

#[tokio::test]
async fn import_runtime_checks_the_source_cache_unless_refresh_is_requested() {
    let runtime = Arc::new(FakeImportRuntime::new(true));
    let (mut engine, mut frontend, _tmp) = import_engine(runtime.clone(), false, archive_sources());
    step_through_image_phases(&mut engine, &mut frontend).await;
    let imports = runtime.imports.lock().unwrap().clone();
    assert_eq!(
        imports.len(),
        1,
        "the acquirer checks full source provenance"
    );
    assert!(!imports[0].refresh, "cached acquisition remains offline");
    assert_eq!(engine.summary().agent_image, StepStatus::Done);

    let runtime = Arc::new(FakeImportRuntime::new(true));
    let (mut engine, mut frontend, _tmp) = import_engine(runtime.clone(), true, archive_sources());
    step_through_image_phases(&mut engine, &mut frontend).await;
    let imports = runtime.imports.lock().unwrap().clone();
    assert_eq!(imports.len(), 1, "--build re-acquires from the source");
    assert!(imports[0].refresh);
    assert_eq!(*runtime.builds.lock().unwrap(), 0);
}

#[tokio::test]
async fn import_runtime_keeps_an_installed_image_when_no_source_is_configured() {
    let runtime = Arc::new(FakeImportRuntime::new(true));
    let (mut engine, mut frontend, _tmp) =
        import_engine(runtime.clone(), false, ImageSources::default());
    step_through_image_phases(&mut engine, &mut frontend).await;
    assert!(runtime.imports.lock().unwrap().is_empty());
    assert_eq!(engine.summary().agent_image, StepStatus::Done);
}

#[tokio::test]
async fn import_runtime_rechecks_changed_locators_with_the_same_kind_and_reference() {
    use crate::data::config::image_source::ImageSourceSpec;
    for sources in [
        [
            ImageSourceSpec::Archive {
                path: "/exports/first.tar".into(),
            },
            ImageSourceSpec::Archive {
                path: "/exports/second.tar".into(),
            },
        ],
        [
            ImageSourceSpec::DockerStore {
                host: Some("unix:///first.sock".into()),
                tls: None,
                reference: None,
            },
            ImageSourceSpec::DockerStore {
                host: Some("unix:///second.sock".into()),
                tls: None,
                reference: None,
            },
        ],
        [
            ImageSourceSpec::Registry {
                registry: Some("first.example".into()),
                reference: None,
            },
            ImageSourceSpec::Registry {
                registry: Some("second.example".into()),
                reference: None,
            },
        ],
    ] {
        let mut fake = FakeImportRuntime::new(true);
        fake.cached_source = sources[0].kind();
        let runtime = Arc::new(fake);
        let (mut engine, mut frontend, _tmp) = import_engine(
            runtime.clone(),
            false,
            ImageSources {
                default: Some(sources[0].clone()),
                images: Default::default(),
                registries: Default::default(),
            },
        );
        let root = engine.session.git_root().to_path_buf();
        engine
            .import_default_agent_image(&mut frontend, &root)
            .await;
        let mut changed_sources = engine.preparer.image_sources().clone();
        changed_sources.default = Some(sources[1].clone());
        engine = engine.with_image_sources(changed_sources);
        engine
            .import_default_agent_image(&mut frontend, &root)
            .await;
        let imports = runtime.imports.lock().unwrap();
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].source, sources[0]);
        assert_eq!(imports[1].source, sources[1]);
        assert!(imports.iter().all(|request| !request.refresh));
    }
}

#[tokio::test]
async fn import_runtime_without_a_source_explains_the_external_build() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut engine, mut frontend, _tmp) =
        import_engine(runtime.clone(), false, ImageSources::default());
    step_through_image_phases(&mut engine, &mut frontend).await;
    assert!(runtime.imports.lock().unwrap().is_empty());
    assert_eq!(*runtime.builds.lock().unwrap(), 0);
    match engine.summary().agent_image {
        StepStatus::Failed(msg) => {
            assert!(msg.contains("does not build"), "{msg}");
            assert!(msg.contains("Dockerfile.claude"), "{msg}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(matches!(
        engine.summary().image_source,
        Some(StepStatus::Failed(_))
    ));
}

#[tokio::test]
async fn import_runtime_reimports_an_image_cached_from_another_source_kind() {
    let mut fake = FakeImportRuntime::new(true);
    fake.cached_source = crate::data::config::image_source::ImageSourceKind::DockerStore;
    let runtime = Arc::new(fake);
    let (mut engine, mut frontend, _tmp) = import_engine(runtime.clone(), false, archive_sources());
    step_through_image_phases(&mut engine, &mut frontend).await;
    let imports = runtime.imports.lock().unwrap().clone();
    assert_eq!(imports.len(), 1, "the configured archive source is used");
    assert_eq!(
        imports[0].source.kind(),
        crate::data::config::image_source::ImageSourceKind::Archive
    );
    assert!(!imports[0].refresh);
    assert_eq!(engine.summary().agent_image, StepStatus::Done);
    // No progress row is left Running once the import finished.
    let mut last = std::collections::BTreeMap::new();
    for (step, status) in &frontend.statuses {
        last.insert(step.clone(), status.clone());
    }
    assert!(
        last.keys().any(|s| s.contains("importing again")),
        "{last:?}"
    );
    assert!(
        last.values().all(|s| *s != StepStatus::Running),
        "rows left running: {last:?}"
    );
}

/// Write `.awman/Dockerfile.<agent>` for each agent so ready discovers it.
pub(crate) fn write_agent_dockerfiles(root: &std::path::Path, agents: &[&str]) {
    let paths = crate::data::repo_dockerfile_paths::RepoDockerfilePaths::new(root);
    for agent in agents {
        let path = paths.agent_dockerfile(agent);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "FROM scratch\n").unwrap();
    }
}

pub(crate) fn docker_store_sources() -> ImageSources {
    ImageSources {
        default: Some(
            crate::data::config::image_source::ImageSourceSpec::DockerStore {
                host: Some("unix:///test/docker.sock".into()),
                tls: None,
                reference: None,
            },
        ),
        images: Default::default(),
        registries: Default::default(),
    }
}

pub(crate) fn last_status(frontend: &FakeReadyFrontend, step: &str) -> Option<StepStatus> {
    frontend
        .statuses
        .iter()
        .rev()
        .find(|(s, _)| s == step)
        .map(|(_, status)| status.clone())
}

#[tokio::test]
async fn import_runtime_build_imports_every_agent_image() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut engine, mut frontend, tmp) =
        import_engine(runtime.clone(), true, docker_store_sources());
    write_agent_dockerfiles(tmp.path(), &["claude", "codex", "gemini"]);
    step_through_image_phases(&mut engine, &mut frontend).await;

    let tags: Vec<String> = runtime
        .imports
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.tag.clone())
        .collect();
    let tag = |a| crate::data::image_tags::agent_image_tag(tmp.path(), a);
    assert_eq!(tags, vec![tag("claude"), tag("codex"), tag("gemini")]);
    assert!(runtime.imports.lock().unwrap().iter().all(|r| r.refresh));
    assert_eq!(*runtime.builds.lock().unwrap(), 0);
    for agent in ["codex", "gemini"] {
        assert_eq!(
            last_status(&frontend, &format!("Import agent image: {agent}")),
            Some(StepStatus::Done)
        );
    }
    assert_eq!(
        last_status(&frontend, "Import agent image: claude"),
        None,
        "the default agent keeps its own summary row"
    );
    assert_eq!(engine.summary().agent_image, StepStatus::Done);
}

#[tokio::test]
async fn import_runtime_without_build_imports_only_the_default_agent() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut engine, mut frontend, tmp) =
        import_engine(runtime.clone(), false, docker_store_sources());
    write_agent_dockerfiles(tmp.path(), &["claude", "codex"]);
    step_through_image_phases(&mut engine, &mut frontend).await;
    let imports = runtime.imports.lock().unwrap();
    assert_eq!(imports.len(), 1);
    assert_eq!(
        imports[0].tag,
        crate::data::image_tags::agent_image_tag(tmp.path(), "claude")
    );
}

#[tokio::test]
async fn import_runtime_build_never_reuses_a_single_image_default_source() {
    use crate::data::config::image_source::ImageSourceSpec;
    // An archive, or a default source pinned to one reference, holds the
    // default agent's image; importing it under codex's tag would be wrong.
    let pinned = ImageSources {
        default: Some(ImageSourceSpec::Registry {
            registry: Some("ghcr.io".into()),
            reference: Some("acme/claude:1".into()),
        }),
        images: Default::default(),
        registries: Default::default(),
    };
    for sources in [archive_sources(), pinned] {
        let runtime = Arc::new(FakeImportRuntime::new(false));
        let (mut engine, mut frontend, tmp) = import_engine(runtime.clone(), true, sources);
        write_agent_dockerfiles(tmp.path(), &["claude", "codex"]);
        step_through_image_phases(&mut engine, &mut frontend).await;
        assert_eq!(runtime.imports.lock().unwrap().len(), 1);
        match last_status(&frontend, "Import agent image: codex") {
            Some(StepStatus::Warn(msg)) => assert!(msg.contains("builtin.images"), "{msg}"),
            other => panic!("expected Warn, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn import_runtime_build_uses_a_per_tag_source_for_other_agents() {
    use crate::data::config::image_source::ImageSourceSpec;
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut engine, mut frontend, tmp) = import_engine(runtime.clone(), true, archive_sources());
    write_agent_dockerfiles(tmp.path(), &["claude", "codex"]);
    let codex_tag = crate::data::image_tags::agent_image_tag(tmp.path(), "codex");
    let codex_source = ImageSourceSpec::Archive {
        path: "/exports/codex.tar".into(),
    };
    let mut sources = engine.preparer.image_sources().clone();
    sources
        .images
        .insert(codex_tag.clone(), codex_source.clone());
    engine = engine.with_image_sources(sources);
    step_through_image_phases(&mut engine, &mut frontend).await;
    let imports = runtime.imports.lock().unwrap();
    assert_eq!(imports.len(), 2);
    assert_eq!(imports[1].tag, codex_tag);
    assert_eq!(imports[1].source, codex_source);
    assert_eq!(
        last_status(&frontend, "Import agent image: codex"),
        Some(StepStatus::Done)
    );
}

#[tokio::test]
async fn import_runtime_provisions_the_dockerfile_its_build_hint_names() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut engine, mut frontend, tmp) =
        import_engine(runtime.clone(), false, ImageSources::default());
    let dockerfile = crate::data::repo_dockerfile_paths::RepoDockerfilePaths::new(tmp.path())
        .agent_dockerfile("claude");
    assert!(!dockerfile.exists());
    step_through_image_phases(&mut engine, &mut frontend).await;
    assert!(
        dockerfile.exists(),
        "the external build instruction names an existing Dockerfile"
    );
    assert_eq!(*runtime.builds.lock().unwrap(), 0);
}

// ── WI 0125: source-matched local builder ──────────────────────────────
//
// A `FakeLocalBuilder` substitutes for `HostLocalImageBuilder`: it is
// scripted per reference string (FIFO per call), never spawns a
// process, and records every probe/inspect/build call so these tests
// run with no Docker/Apple daemon and no network. `builder_engine`
// enables `builtin.builder.enabled` through a real `.awman/config.json`
// (the only way to set the private `builder_enabled` field) and injects
// private, per-test build-lock/receipt roots so no two tests, and no
// real `$HOME`, ever share state.

use crate::engine::oci::SourceImage;

pub(crate) fn built_identity(
    hex: char,
    platform: crate::data::oci_identity::OciPlatform,
) -> BuiltImageIdentity {
    let digest =
        crate::data::oci_identity::Digest::parse(&format!("sha256:{}", hex.to_string().repeat(64)))
            .unwrap();
    BuiltImageIdentity::new(Some(digest), None, platform).unwrap()
}

/// A `builtin.imageSource` naming an explicit local Unix socket. Explicit
/// (never the platform default): under `cfg(test)` the default socket is
/// disabled unless `AWMAN_TEST_DOCKER` is set (test isolation), and an
/// explicit `host` is the one path WI 0125 allows a build into.
pub(crate) fn local_docker_sources(socket: &std::path::Path) -> ImageSources {
    ImageSources {
        default: Some(
            crate::data::config::image_source::ImageSourceSpec::DockerStore {
                host: Some(format!("unix://{}", socket.display())),
                tls: None,
                reference: None,
            },
        ),
        images: Default::default(),
        registries: Default::default(),
    }
}

/// A `ReadyEngine` with `builtin.builder.enabled` set (the only way to
/// set it: it is read once, from real repo config, in `ReadyEngine::new`)
/// and a `FakeLocalBuilder` injected in place of the host CLI builder.
/// Build-lock and receipt roots are private per-test temp directories,
/// passed back to the caller so a second engine can share them (retry
/// scenarios) or each test can use its own (isolation).
pub(crate) fn builder_engine(
    runtime: Arc<FakeImportRuntime>,
    build: bool,
    no_cache: bool,
    sources: ImageSources,
    local_builder: Arc<FakeLocalBuilder>,
) -> (
    ReadyEngine,
    FakeReadyFrontend,
    tempfile::TempDir,
    ImageBuildLockStore,
    PendingBuiltImageStore,
) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
    let awman_dir = tmp.path().join(".awman");
    std::fs::create_dir_all(&awman_dir).unwrap();
    std::fs::write(awman_dir.join("Dockerfile.claude"), "FROM scratch\n").unwrap();
    std::fs::write(
        awman_dir.join("config.json"),
        r#"{"builtin":{"builder":{"enabled":true}}}"#,
    )
    .unwrap();
    let resolver = StaticGitRootResolver::new(tmp.path());
    let session = Arc::new(
        crate::data::session::Session::open(
            tmp.path().to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap(),
    );
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(tmp.path()),
    ));
    let dyn_runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = runtime;
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay.clone(),
        dyn_runtime.clone(),
    ));
    let locks = ImageBuildLockStore::new(tmp.path().join("locks"));
    let receipts = PendingBuiltImageStore::new(tmp.path().join("receipts"));
    let engine = ReadyEngine::new(
        ReadyEngine::builtin_inputs(&session, dyn_runtime.as_ref()).unwrap(),
        crate::data::config::global::GlobalConfig::data_home_with(session.effective_config().env())
            .unwrap(),
        session,
        Arc::new(GitEngine::new()),
        overlay,
        dyn_runtime,
        agent_engine,
        ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build,
            no_cache,
            allow_docker: false,
            non_interactive: true,
            env_passthrough: None,
        },
    )
    .with_image_sources(sources)
    .with_local_builder(local_builder as Arc<dyn LocalImageBuilder>)
    .with_build_stores(locks.clone(), receipts.clone());
    let frontend = FakeReadyFrontend {
        create_dockerfile: false,
        run_audit: false,
        phases: Vec::new(),
        statuses: Vec::new(),
        last_dockerfile_prompt_path: None,
        messages: Vec::new(),
    };
    (engine, frontend, tmp, locks, receipts)
}

/// Reopen a `builder_engine` at the same `git_root`/sources but with a
/// fresh fake builder/runtime, sharing one build-lock/receipt root — a
/// second `awman ready` invocation after the first, for retry tests.
pub(crate) fn reopen_builder_engine(
    git_root: &std::path::Path,
    runtime: Arc<FakeImportRuntime>,
    build: bool,
    sources: ImageSources,
    local_builder: Arc<FakeLocalBuilder>,
    locks: ImageBuildLockStore,
    receipts: PendingBuiltImageStore,
) -> (ReadyEngine, FakeReadyFrontend) {
    let resolver = StaticGitRootResolver::new(git_root);
    let session = Arc::new(
        crate::data::session::Session::open(
            git_root.to_path_buf(),
            &resolver,
            SessionOpenOptions::default(),
        )
        .unwrap(),
    );
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        crate::data::fs::auth_paths::AuthPathResolver::at_home(git_root),
    ));
    let dyn_runtime: Arc<dyn crate::engine::agent_runtime::AgentRuntimeEngine> = runtime;
    let agent_engine = Arc::new(crate::engine::agent::AgentEngine::new(
        overlay.clone(),
        dyn_runtime.clone(),
    ));
    let engine = ReadyEngine::new(
        ReadyEngine::builtin_inputs(&session, dyn_runtime.as_ref()).unwrap(),
        crate::data::config::global::GlobalConfig::data_home_with(session.effective_config().env())
            .unwrap(),
        session,
        Arc::new(GitEngine::new()),
        overlay,
        dyn_runtime,
        agent_engine,
        ReadyEngineOptions {
            agent: AgentName::new("claude").unwrap(),
            refresh: false,
            build,
            no_cache: false,
            allow_docker: false,
            non_interactive: true,
            env_passthrough: None,
        },
    )
    .with_image_sources(sources)
    .with_local_builder(local_builder as Arc<dyn LocalImageBuilder>)
    .with_build_stores(locks, receipts);
    let frontend = FakeReadyFrontend {
        create_dockerfile: false,
        run_audit: false,
        phases: Vec::new(),
        statuses: Vec::new(),
        last_dockerfile_prompt_path: None,
        messages: Vec::new(),
    };
    (engine, frontend)
}

/// Script a `FakeLocalBuilder` for the common "nothing built yet" path:
/// the agent and project references are both absent from the source
/// store, so the engine builds the base then the agent image.
pub(crate) fn script_missing_image_build(
    builder: &FakeLocalBuilder,
    project_tag: &str,
    agent_tag: &str,
    base_id: BuiltImageIdentity,
    agent_id: BuiltImageIdentity,
) {
    builder.script_inspect(agent_tag, Ok(SourceImage::Absent));
    builder.script_inspect(project_tag, Ok(SourceImage::Absent));
    builder.script_build(project_tag, Ok(base_id.clone()));
    builder.script_inspect(project_tag, Ok(SourceImage::Present(base_id)));
    builder.script_build(agent_tag, Ok(agent_id));
}

/// The audit-rebuild path (a changed `Dockerfile.dev` after the sanctioned
/// host-agent audit) uses the local builder exactly like a plan-driven
/// build: this is isolated to `RebuildingAfterAudit` itself (private
/// fields set directly) so it does not depend on a real agent binary.
#[tokio::test]
async fn builder_audit_rebuild_rebuilds_the_changed_project_image() {
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let runtime = Arc::new(FakeImportRuntime::new(true));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let tmp0 = tempfile::tempdir().unwrap();
    let socket = tmp0.path().join("docker.sock");
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        local_docker_sources(&socket),
        local_builder.clone(),
    );
    let git_root = engine.session.git_root().to_path_buf();
    let dockerfile = git_root.join("Dockerfile.dev");
    let project_tag = crate::data::image_tags::project_image_tag(&git_root);
    let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
    let base_id = built_identity('9', platform.clone());
    let agent_id = built_identity('0', platform);
    // The rebuild forces `options.build = true` internally, which skips
    // both the agent-reference inspect and the `base_exists` inspect —
    // only the post-build verification inspect is scripted.
    local_builder.script_build(&project_tag, Ok(base_id.clone()));
    local_builder.script_inspect(&project_tag, Ok(SourceImage::Present(base_id)));
    local_builder.script_build(&agent_tag, Ok(agent_id));

    // Drive directly to `RebuildingAfterAudit` with an audit that ran
    // and a Dockerfile that changed, bypassing the real host-agent ping
    // and the audit container the ordinary phase sequence would need.
    engine.summary.audit = StepStatus::Done;
    engine.pre_audit_dockerfile_hash = Some(0);
    std::fs::write(&dockerfile, "FROM scratch\nRUN echo changed\n").unwrap();
    engine.phase = ReadyPhase::RebuildingAfterAudit;

    engine.step(&mut frontend).await.unwrap();

    assert_eq!(engine.phase(), &ReadyPhase::Complete);
    assert_eq!(engine.summary().image_rebuild, StepStatus::Done);
    let builds = local_builder.build_calls();
    assert_eq!(
        builds
            .iter()
            .map(|r| r.reference.clone())
            .collect::<Vec<_>>(),
        vec![project_tag, agent_tag]
    );
}

#[tokio::test]
async fn incompatible_builder_warning_is_not_repeated_by_the_audit_phase() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, _tmp, _, _) = builder_engine(
        runtime.clone(),
        false,
        false,
        archive_sources(),
        builder.clone(),
    );
    step_through_image_phases(&mut engine, &mut frontend).await;
    engine.summary.audit = StepStatus::Done;
    engine.pre_audit_dockerfile_hash = Some(0);
    engine.phase = ReadyPhase::RebuildingAfterAudit;
    engine.step(&mut frontend).await.unwrap();
    assert_eq!(engine.phase(), &ReadyPhase::Complete);
    assert_eq!(engine.summary().image_rebuild, StepStatus::Done);
    assert_eq!(runtime.imports.lock().unwrap().len(), 2);
    assert_eq!(
        frontend
            .warning_texts()
            .iter()
            .filter(|text| text.contains("cannot build Dockerfiles"))
            .count(),
        1
    );
    assert_eq!(engine.summary().warnings().count(), 1);
    assert_eq!(builder.inspect_count(), 0);
    assert!(builder.build_calls().is_empty());
}

#[tokio::test]
async fn audit_rebuild_reports_configured_source_resolution_failure() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let builder = Arc::new(FakeLocalBuilder::new());
    let sources = ImageSources {
        default: Some(
            crate::data::config::image_source::ImageSourceSpec::DockerStore {
                host: Some("tcp://build.example:2375".into()),
                tls: None,
                reference: None,
            },
        ),
        ..Default::default()
    };
    let (mut engine, mut frontend, _tmp, _, _) =
        builder_engine(runtime.clone(), false, false, sources, builder.clone());
    engine.summary.audit = StepStatus::Done;
    engine.pre_audit_dockerfile_hash = Some(0);
    engine.phase = ReadyPhase::RebuildingAfterAudit;
    engine.step(&mut frontend).await.unwrap();
    assert!(
        matches!(engine.summary().image_rebuild, StepStatus::Failed(ref text) if text.contains("audit rebuild"))
    );
    let summary = engine.summary();
    assert_eq!(summary.image_results.len(), 1);
    assert!(
        matches!(summary.image_results[0].build, StepStatus::Failed(ref text) if text.contains("tls"))
    );
    assert_eq!(summary.image_results[0].import, StepStatus::Skipped);
    assert!(frontend.warning_texts().is_empty());
    assert!(runtime.imports.lock().unwrap().is_empty());
    assert_eq!(builder.probe_count(), 0);
    assert_eq!(builder.inspect_count(), 0);
    assert!(builder.build_calls().is_empty());
}

/// WI 0126 F2: each missing non-default image records what `ready --build`
/// would do for it, from that image's own source and builder selection —
/// not one verb chosen for the whole summary.
#[tokio::test]
async fn missing_non_default_images_record_what_ready_would_do_for_each() {
    use crate::data::config::image_source::ImageSourceSpec;
    use crate::data::ready_image::{MissingAgentImage, ReadyImagePlan};
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut engine, mut frontend, tmp, _locks, _receipts) = builder_engine(
        runtime,
        false,
        false,
        ImageSources::default(),
        Arc::new(FakeLocalBuilder::new()),
    );
    let root = tmp.path().to_path_buf();
    for agent in ["codex", "gemini", "cline"] {
        std::fs::write(
            root.join(format!(".awman/Dockerfile.{agent}")),
            "FROM scratch\n",
        )
        .unwrap();
    }
    let tag = |agent: &str| crate::data::image_tags::agent_image_tag(&root, agent);
    engine.preparer.replace_sources(ImageSources {
        default: None,
        images: [
            (
                tag("codex"),
                ImageSourceSpec::DockerStore {
                    host: Some(format!("unix://{}", root.join("docker.sock").display())),
                    tls: None,
                    reference: None,
                },
            ),
            (
                tag("gemini"),
                ImageSourceSpec::Registry {
                    registry: Some("ghcr.io".into()),
                    reference: Some("team/gemini:1".into()),
                },
            ),
        ]
        .into_iter()
        .collect(),
        registries: Default::default(),
    });
    // The default image is in place; only the non-default ones are missing.
    engine.summary.base_image = StepStatus::Skipped;
    engine.summary.agent_image = StepStatus::Done;
    engine.phase = ReadyPhase::CheckingNonDefaultAgents;
    engine.step(&mut frontend).await.unwrap();

    let mut missing = engine.summary().missing_images.clone();
    missing.sort_by(|a, b| a.agent.cmp(&b.agent));
    let plan = |agent: &str, plan| MissingAgentImage {
        agent: agent.into(),
        plan,
    };
    assert_eq!(
        missing,
        [
            plan("cline", ReadyImagePlan::ConfigureSource),
            plan("codex", ReadyImagePlan::BuildAndImport),
            plan("gemini", ReadyImagePlan::Import),
        ]
    );
    let hint = engine.summary().hint().unwrap();
    assert!(hint.contains("to import gemini."), "{hint}");
    assert!(hint.contains("to build and import codex."), "{hint}");
    assert!(
        hint.contains("configure an image source for cline"),
        "{hint}"
    );
}

#[tokio::test]
async fn missing_digest_pinned_local_image_hint_requires_writable_output() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut engine, mut frontend, tmp, _, _) = builder_engine(
        runtime,
        false,
        false,
        ImageSources::default(),
        Arc::new(FakeLocalBuilder::new()),
    );
    std::fs::write(tmp.path().join(".awman/Dockerfile.codex"), "FROM scratch\n").unwrap();
    let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "codex");
    engine.preparer.replace_sources(ImageSources {
        images: [(
            tag,
            crate::data::config::image_source::ImageSourceSpec::DockerStore {
                host: Some("unix:///test/docker.sock".into()),
                tls: None,
                reference: Some(format!("team/agent@sha256:{}", "a".repeat(64))),
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    });
    engine.summary.base_image = StepStatus::Skipped;
    engine.summary.agent_image = StepStatus::Done;
    engine.phase = ReadyPhase::CheckingNonDefaultAgents;
    engine.step(&mut frontend).await.unwrap();
    let hint = engine.summary().hint().unwrap();
    assert!(hint.contains("writable"), "{hint}");
    assert!(!hint.contains("to build and import codex"), "{hint}");
}

/// A `builtin` block applies only to the builtin runtime. On a runtime that
/// builds its own images it is never resolved, so an invalid or widening one
/// cannot fail `awman ready` or API session setup there; on the builtin
/// runtime it still fails closed.
#[test]
fn builtin_inputs_are_resolved_only_for_an_importing_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".awman")).unwrap();
    std::fs::write(
        tmp.path().join(".awman/config.json"),
        r#"{"builtin":{"vcpus":0,"network":{"mode":"public"}}}"#,
    )
    .unwrap();
    let session = Session::for_tests(tmp.path());
    assert!(session.effective_config().builtin_settings().is_err());

    let builds = FakeKitRuntime::new();
    assert!(matches!(
        ReadyEngine::builtin_inputs(&session, &builds),
        Ok(None)
    ));
    let imports = FakeImportRuntime::new(true);
    assert!(ReadyEngine::builtin_inputs(&session, &imports).is_err());
}

#[tokio::test]
async fn custom_without_ping_skips_the_host_seam() {
    struct NeverPing;
    #[async_trait::async_trait]
    impl ReadyHostPing for NeverPing {
        async fn ping(&self, _: &crate::engine::agent::ResolvedAgent) -> LocalAgentPingResult {
            panic!("a definition with no ping must never reach the host seam")
        }
    }
    let (mut engine, mut frontend, _temp) = make_engine_and_frontend(false, false);
    let mut def = crate::engine::agent::custom::crush_fixture();
    def.ping_argv = None;
    engine.definition = Some(crate::engine::agent::ResolvedAgent::for_test(def));
    engine.phase = ReadyPhase::CheckingLocalAgent;
    engine = engine.with_host_ping(Arc::new(NeverPing));
    engine.step(&mut frontend).await.unwrap();
    assert_eq!(engine.summary.local_agent, StepStatus::Skipped);
    assert!(frontend
        .messages
        .iter()
        .any(|m| m.text == "no [ping] command in /fixtures/agents/crush.toml; host check skipped"));
}

#[tokio::test]
async fn written_custom_dockerfile_forces_ready_build_with_an_existing_image() {
    let (mut engine, mut frontend, _temp) = make_engine_and_frontend(false, false);
    let mut runtime = FakeImportRuntime::new(true);
    runtime.caps.image_acquisition = crate::engine::agent_runtime::ImageAcquisition::Build;
    let runtime = Arc::new(runtime);
    engine.runtime = runtime.clone();
    engine.options.agent = AgentName::new("crush").unwrap();
    engine.options.build = false;
    let def = crate::engine::agent::custom::crush_fixture();
    engine.definition = Some(crate::engine::agent::ResolvedAgent::for_test(def.clone()));
    engine.phase = ReadyPhase::BuildingAgentImage;
    engine.step(&mut frontend).await.unwrap();
    assert_eq!(*runtime.builds.lock().unwrap(), 1);
    let StepStatus::Failed(message) = &engine.summary.agent_image else {
        panic!("scripted build must fail")
    };
    assert!(message.contains("/fixtures/agents/crush.toml"), "{message}");
    assert!(message.contains("smoke binary 'crush'"), "{message}");
    assert!(
        message.contains("npm install -g --prefix=/usr/local @charmland/crush"),
        "{message}"
    );
    engine.phase = ReadyPhase::BuildingAgentImage;
    engine.step(&mut frontend).await.unwrap();
    assert_eq!(
        *runtime.builds.lock().unwrap(),
        1,
        "unchanged recipe keeps the image"
    );
    let mut changed = def;
    if let crate::engine::agent::AgentOrigin::Custom { sha256, .. } = &mut changed.origin {
        *sha256 = "new-hash".into();
    }
    engine.definition = Some(crate::engine::agent::ResolvedAgent::for_test(changed));
    engine.phase = ReadyPhase::BuildingAgentImage;
    engine.step(&mut frontend).await.unwrap();
    assert_eq!(
        *runtime.builds.lock().unwrap(),
        2,
        "new recipe forces a build"
    );
}

#[tokio::test]
async fn approved_custom_sandbox_ready_fails_without_emitting_a_kit() {
    let (mut engine, mut frontend, temp) = make_engine_and_frontend(false, false);
    engine.session = Arc::new(Session::for_tests_isolated(
        temp.path(),
        &temp.path().join("global"),
    ));
    crate::engine::agent::custom::install_crush_fixture(temp.path(), engine.session.env());
    let runtime = Arc::new(FakeKitRuntime::new());
    engine.runtime = runtime.clone();
    engine.agent_engine = Arc::new(AgentEngine::new(
        engine.overlay_engine.clone(),
        runtime.clone(),
    ));
    engine.options.agent = AgentName::new("crush").unwrap();
    let error = engine.step(&mut frontend).await.unwrap_err();
    assert!(matches!(
        error,
        EngineError::CustomAgentRuntimeUnsupported { .. }
    ));
    assert!(matches!(engine.summary.agent_image, StepStatus::Failed(_)));
    assert!(runtime.ready_calls.lock().unwrap().is_empty());
    assert!(!crate::data::RepoDockerfilePaths::new(temp.path())
        .agent_dockerfile("crush")
        .exists());
}
