use super::*;
/// Disabled mode (no `.awman/config.json`, so `builder_enabled` is the
/// documented default `false`) must never probe, inspect or build, even
/// against a source the builder would otherwise treat as local/Docker —
/// "unchanged disabled mode".
#[tokio::test]
async fn disabled_builder_never_calls_the_local_builder() {
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut engine, mut frontend, _tmp) =
        import_engine(runtime.clone(), false, docker_store_sources());
    engine = engine.with_local_builder(local_builder.clone() as Arc<dyn LocalImageBuilder>);
    step_through_image_phases(&mut engine, &mut frontend).await;
    assert_eq!(local_builder.probe_count(), 0);
    assert_eq!(local_builder.inspect_count(), 0);
    assert!(local_builder.build_calls().is_empty());
    // Import-only behaviour is unchanged: it still imports from the
    // configured source, with an explicit fake endpoint under test isolation.
    assert_eq!(engine.summary().agent_image, StepStatus::Done);
}

/// Ordinary missing-image readiness: nothing is built or installed yet,
/// so plain `ready` (no `--build`) builds the project base before the
/// agent image — base-before-agent ordering — then imports the agent
/// build's exact content with forced refresh.
#[tokio::test]
async fn builder_builds_base_then_agent_on_a_missing_image() {
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        ImageSources::default(),
        local_builder.clone(),
    );
    let socket = tmp.path().join("docker.sock");
    engine.preparer.image_sources = local_docker_sources(&socket);
    let project_tag = crate::data::image_tags::project_image_tag(tmp.path());
    let agent_tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    let base_id = built_identity('a', platform.clone());
    let agent_id = built_identity('b', platform);
    script_missing_image_build(
        &local_builder,
        &project_tag,
        &agent_tag,
        base_id,
        agent_id.clone(),
    );

    step_through_image_phases(&mut engine, &mut frontend).await;

    let builds = local_builder.build_calls();
    assert_eq!(
        builds
            .iter()
            .map(|r| r.reference.clone())
            .collect::<Vec<_>>(),
        vec![project_tag.clone(), agent_tag.clone()],
        "the project base image must be built before the agent image"
    );

    let summary = engine.summary();
    assert_eq!(summary.base_image_results.len(), 1);
    assert_eq!(summary.base_image_results[0].tag, project_tag);
    assert_eq!(summary.base_image_results[0].build, StepStatus::Done);
    assert_eq!(summary.image_results.len(), 1);
    assert_eq!(summary.image_results[0].build, StepStatus::Done);
    assert_eq!(summary.image_results[0].import, StepStatus::Done);
    assert!(summary.is_ready());
    assert_eq!(summary.outcome_line(), "awman is ready.");

    let imports = runtime.imports.lock().unwrap();
    assert_eq!(imports.len(), 1);
    assert!(
        imports[0].refresh,
        "a fresh build must force re-acquisition"
    );
    assert_eq!(
        imports[0].expected_built_identity,
        Some(agent_id),
        "the import must be bound to the exact content the build produced"
    );
    assert_eq!(
        last_status(
            &frontend,
            &format!(
                "Build base image {project_tag} in docker unix://{}",
                socket.display()
            ),
        ),
        Some(StepStatus::Done),
        "the base-image build step must be reported per the ReadyStep::BuildBaseImageIn label"
    );
}

/// `--build` rebuilds even an already-installed image, in its source
/// store, and the import is still refreshed with the exact content the
/// build produced.
#[tokio::test]
async fn builder_forced_build_rebuilds_an_already_installed_image() {
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let runtime = Arc::new(FakeImportRuntime::new(true));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("docker.sock");
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        true,
        false,
        local_docker_sources(&socket),
        local_builder.clone(),
    );
    // `builder_engine` already wrote Dockerfile.dev/.awman inside its own
    // tmpdir; use that one instead of the throwaway `tmp` above.
    let git_root = engine.session.git_root().to_path_buf();
    let project_tag = crate::data::image_tags::project_image_tag(&git_root);
    let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
    let base_id = built_identity('c', platform.clone());
    let agent_id = built_identity('d', platform);
    local_builder.script_build(&project_tag, Ok(base_id.clone()));
    local_builder.script_inspect(&project_tag, Ok(SourceImage::Present(base_id)));
    local_builder.script_build(&agent_tag, Ok(agent_id.clone()));

    step_through_image_phases(&mut engine, &mut frontend).await;

    let builds = local_builder.build_calls();
    assert_eq!(
        builds
            .iter()
            .map(|r| r.reference.clone())
            .collect::<Vec<_>>(),
        vec![project_tag, agent_tag]
    );
    let imports = runtime.imports.lock().unwrap();
    assert_eq!(imports.len(), 1);
    assert!(
        imports[0].refresh,
        "--build must re-acquire even though the runtime already has the image"
    );
    assert_eq!(imports[0].expected_built_identity, Some(agent_id));
}

/// `--no-cache` is forwarded onto every build request it schedules
/// (base and agent), exactly as the `build`/`no_cache` flags already
/// behave on the Docker/Apple runtimes — it is not an independent build
/// trigger.
#[tokio::test]
async fn builder_forwards_no_cache_to_every_build_request() {
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let tmp0 = tempfile::tempdir().unwrap();
    let socket = tmp0.path().join("docker.sock");
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        true,
        local_docker_sources(&socket),
        local_builder.clone(),
    );
    let git_root = engine.session.git_root().to_path_buf();
    let project_tag = crate::data::image_tags::project_image_tag(&git_root);
    let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
    script_missing_image_build(
        &local_builder,
        &project_tag,
        &agent_tag,
        built_identity('e', platform.clone()),
        built_identity('f', platform),
    );

    step_through_image_phases(&mut engine, &mut frontend).await;

    let builds = local_builder.build_calls();
    assert_eq!(builds.len(), 2);
    assert!(
        builds.iter().all(|r| r.no_cache),
        "no_cache must reach every scheduled build request: {builds:?}"
    );
}

/// Two non-default agents built into the same store as the default agent
/// (`ready --build`) share one base-image build: the base is deduped per
/// store within one ready plan, not rebuilt per agent.
#[tokio::test]
async fn builder_dedups_the_base_build_across_multiple_agents_in_one_store() {
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let tmp0 = tempfile::tempdir().unwrap();
    let socket = tmp0.path().join("docker.sock");
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        true,
        false,
        local_docker_sources(&socket),
        local_builder.clone(),
    );
    let git_root = engine.session.git_root().to_path_buf();
    write_agent_dockerfiles(&git_root, &["claude", "codex", "gemini"]);
    let project_tag = crate::data::image_tags::project_image_tag(&git_root);
    // `--build` skips the `base_exists` inspect (it always rebuilds), so
    // only the post-build verification inspect is scripted.
    let base_id = built_identity('1', platform.clone());
    local_builder.script_build(&project_tag, Ok(base_id.clone()));
    local_builder.script_inspect(&project_tag, Ok(SourceImage::Present(base_id)));
    for (agent, hex) in [("claude", '2'), ("codex", '3'), ("gemini", '4')] {
        let tag = crate::data::image_tags::agent_image_tag(&git_root, agent);
        local_builder.script_build(&tag, Ok(built_identity(hex, platform.clone())));
    }

    step_through_image_phases(&mut engine, &mut frontend).await;

    let builds = local_builder.build_calls();
    let base_builds = builds.iter().filter(|r| r.reference == project_tag).count();
    assert_eq!(
        base_builds, 1,
        "base must be built exactly once per store: {builds:?}"
    );
    for agent in ["claude", "codex", "gemini"] {
        let tag = crate::data::image_tags::agent_image_tag(&git_root, agent);
        assert!(
            builds.iter().any(|r| r.reference == tag),
            "{agent}'s image must have been built; builds: {builds:?}"
        );
    }
    assert_eq!(engine.summary().base_image_results.len(), 1);
}

/// A1: two repos (sessions) in one process, sharing the same local
/// builder resource, must each act on only their own session's builder
/// settings — a disabled-builder session must not leak the other
/// session's enablement (or the reverse), regardless of construction
/// order or which session runs first.
#[tokio::test]
async fn two_sessions_with_opposite_builder_settings_each_see_only_their_own() {
    use crate::data::config::image_source::ImageSourceSpec;
    let apple_sources = ImageSources {
        default: Some(ImageSourceSpec::AppleStore { reference: None }),
        images: Default::default(),
        registries: Default::default(),
    };
    let shared_local_builder = Arc::new(FakeLocalBuilder::new());

    // Session A: no repo config.json, so the builder is disabled by
    // default. Even though it is wired to the same local-builder
    // instance session B will use, it must never call it.
    let runtime_a = Arc::new(FakeImportRuntime::new(false));
    let (mut engine_a, mut frontend_a, _tmp_a) =
        import_engine(runtime_a.clone(), false, apple_sources.clone());
    engine_a =
        engine_a.with_local_builder(shared_local_builder.clone() as Arc<dyn LocalImageBuilder>);
    step_through_image_phases(&mut engine_a, &mut frontend_a).await;
    assert_eq!(
        shared_local_builder.probe_count(),
        0,
        "session A's disabled builder must not have probed"
    );
    assert!(
        shared_local_builder.build_calls().is_empty(),
        "session A's disabled builder must not have built"
    );
    assert_eq!(engine_a.summary().agent_image, StepStatus::Done);

    // Session B: its own repo's config.json turns the builder on,
    // sharing the exact local-builder instance A just ran against with
    // zero calls. B's own enablement must now drive it.
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let runtime_b = Arc::new(FakeImportRuntime::new(false));
    let (mut engine_b, mut frontend_b, tmp_b, _locks, _receipts) = builder_engine(
        runtime_b.clone(),
        false,
        false,
        apple_sources,
        shared_local_builder.clone(),
    );
    let project_tag = crate::data::image_tags::project_image_tag(tmp_b.path());
    let agent_tag = crate::data::image_tags::agent_image_tag(tmp_b.path(), "claude");
    script_missing_image_build(
        &shared_local_builder,
        &project_tag,
        &agent_tag,
        built_identity('a', platform.clone()),
        built_identity('b', platform),
    );
    step_through_image_phases(&mut engine_b, &mut frontend_b).await;
    let builds = shared_local_builder.build_calls();
    assert_eq!(
        builds
            .iter()
            .map(|r| r.reference.clone())
            .collect::<Vec<_>>(),
        vec![project_tag, agent_tag],
        "session B's own enabled builder must drive the shared instance \
             exactly once each, unaffected by A ever having used it"
    );
    assert!(engine_b.summary().is_ready());
}

/// The same ready plan can build into two different source stores (one
/// agent on a local Docker Engine, another on the local Apple Containers
/// store): each store gets its own base-image build, independently.
#[tokio::test]
async fn builder_builds_a_base_per_store_across_different_builders() {
    use crate::data::config::image_source::ImageSourceSpec;
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let tmp0 = tempfile::tempdir().unwrap();
    let socket = tmp0.path().join("docker.sock");
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        true,
        false,
        local_docker_sources(&socket),
        local_builder.clone(),
    );
    let git_root = engine.session.git_root().to_path_buf();
    write_agent_dockerfiles(&git_root, &["claude", "codex"]);
    let codex_tag = crate::data::image_tags::agent_image_tag(&git_root, "codex");
    engine.preparer.image_sources.images.insert(
        codex_tag.clone(),
        ImageSourceSpec::AppleStore { reference: None },
    );
    let project_tag = crate::data::image_tags::project_image_tag(&git_root);
    // `--build` skips the `base_exists` inspect; one post-build
    // verification inspect per store (Docker, then Apple).
    for hex in ['5', '6'] {
        let base_id = built_identity(hex, platform.clone());
        local_builder.script_build(&project_tag, Ok(base_id.clone()));
        local_builder.script_inspect(&project_tag, Ok(SourceImage::Present(base_id)));
    }
    let claude_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
    local_builder.script_build(&claude_tag, Ok(built_identity('7', platform.clone())));
    local_builder.script_build(&codex_tag, Ok(built_identity('8', platform)));

    step_through_image_phases(&mut engine, &mut frontend).await;

    let summary = engine.summary();
    assert_eq!(
        summary.base_image_results.len(),
        2,
        "each store gets its own base-image row: {:?}",
        summary.base_image_results
    );
    let builds = local_builder.build_calls();
    assert_eq!(
        builds.iter().filter(|r| r.reference == project_tag).count(),
        2,
        "the base must be built once per store: {builds:?}"
    );
}

/// The base image a non-default agent's store needs is as optional as that
/// agent's image (WI 0125: preserve non-default-agent failure aggregation).
/// Its failed build is reported, but `ready` is still ready once every
/// required image is.
#[tokio::test]
async fn a_failed_base_for_a_non_default_agents_store_does_not_fail_ready() {
    use crate::data::config::image_source::ImageSourceSpec;
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let tmp0 = tempfile::tempdir().unwrap();
    let socket = tmp0.path().join("docker.sock");
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        true,
        false,
        local_docker_sources(&socket),
        local_builder.clone(),
    );
    let git_root = engine.session.git_root().to_path_buf();
    write_agent_dockerfiles(&git_root, &["claude", "codex"]);
    let codex_tag = crate::data::image_tags::agent_image_tag(&git_root, "codex");
    engine.preparer.image_sources.images.insert(
        codex_tag.clone(),
        ImageSourceSpec::AppleStore { reference: None },
    );
    let project_tag = crate::data::image_tags::project_image_tag(&git_root);
    // Docker store (claude, the default agent): base builds and verifies.
    let base_id = built_identity('5', platform.clone());
    local_builder.script_build(&project_tag, Ok(base_id.clone()));
    local_builder.script_inspect(&project_tag, Ok(SourceImage::Present(base_id)));
    // Apple store (codex only): the base build fails.
    local_builder.script_build(&project_tag, Err("apple base build failed".into()));
    let claude_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
    local_builder.script_build(&claude_tag, Ok(built_identity('7', platform)));

    step_through_image_phases(&mut engine, &mut frontend).await;

    let summary = engine.summary();
    assert_eq!(
        summary.base_image_results.len(),
        2,
        "{:?}",
        summary.base_image_results
    );
    let (required, optional): (Vec<_>, Vec<_>) =
        summary.base_image_results.iter().partition(|b| !b.optional);
    assert_eq!(required.len(), 1);
    assert_eq!(required[0].build, StepStatus::Done);
    assert_eq!(optional.len(), 1);
    assert!(matches!(optional[0].build, StepStatus::Failed(_)));
    assert!(
        summary.is_ready(),
        "only a non-default agent's store failed: {:?}",
        summary.not_ready_reason()
    );
}

/// An incompatible source (archive) with an attempted build (missing
/// image) warns exactly once, invokes the local builder zero times, and
/// still imports normally from the configured source.
#[tokio::test]
async fn builder_incompatible_source_warns_once_and_imports_normally() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        archive_sources(),
        local_builder.clone(),
    );

    step_through_image_phases(&mut engine, &mut frontend).await;

    assert_eq!(local_builder.probe_count(), 0);
    assert_eq!(local_builder.inspect_count(), 0);
    assert!(local_builder.build_calls().is_empty());
    let summary = engine.summary();
    assert_eq!(summary.image_results.len(), 1);
    assert_eq!(summary.image_results[0].warnings.len(), 1);
    match &summary.image_results[0].warnings[0] {
        ReadyImageWarning::IncompatibleBuilder {
            tag, source_kind, ..
        } => {
            assert_eq!(
                *source_kind,
                crate::data::config::image_source::ImageSourceKind::Archive
            );
            assert_eq!(tag, &summary.image_results[0].tag);
        }
        other => panic!("expected IncompatibleBuilder, got {other:?}"),
    }
    assert_eq!(
        frontend
            .warning_texts()
            .iter()
            .filter(|t| t.contains("cannot build Dockerfiles"))
            .count(),
        1,
        "the warning must be written to the frontend exactly once: {:?}",
        frontend.warning_texts()
    );
    assert_eq!(runtime.imports.lock().unwrap().len(), 1);
    assert_eq!(summary.image_results[0].import, StepStatus::Done);
}

/// With the image already installed and no `--build`, nothing attempts
/// a build, so the incompatible-source warning is never emitted at all —
/// the non-build-path silence half of the same requirement.
#[tokio::test]
async fn builder_incompatible_source_is_silent_on_a_non_build_path() {
    let runtime = Arc::new(FakeImportRuntime::new(true));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        archive_sources(),
        local_builder.clone(),
    );

    step_through_image_phases(&mut engine, &mut frontend).await;

    assert!(local_builder.build_calls().is_empty());
    let summary = engine.summary();
    assert!(
        summary.image_results[0].warnings.is_empty(),
        "no warning when no build was attempted: {:?}",
        summary.image_results[0].warnings
    );
    assert!(frontend
        .warning_texts()
        .iter()
        .all(|t| !t.contains("cannot build Dockerfiles")));
}

/// A compatible but unreachable builder, with nothing installed yet, is
/// a build failure — never a silent import of stale or no content.
#[tokio::test]
async fn builder_unavailable_with_nothing_installed_fails_the_build() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let local_builder = Arc::new(FakeLocalBuilder::unavailable("daemon unreachable"));
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        ImageSources::default(),
        local_builder.clone(),
    );
    let socket = engine.session.git_root().join("docker.sock");
    engine.preparer.image_sources = local_docker_sources(&socket);

    step_through_image_phases(&mut engine, &mut frontend).await;

    let summary = engine.summary();
    assert_eq!(summary.image_results.len(), 1);
    match &summary.image_results[0].build {
        StepStatus::Failed(msg) => assert!(msg.contains("daemon unreachable"), "{msg}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(summary.image_results[0].import, StepStatus::Skipped);
    assert!(
        runtime.imports.lock().unwrap().is_empty(),
        "an unreachable builder with nothing installed must never import"
    );
    assert!(!summary.is_ready());
    // `agent_image`/`image_source` (set by `record_import_failure`) are
    // checked ahead of the per-image `image_results` rows, so the
    // top-level stage's message is what surfaces here.
    assert!(
        summary
            .not_ready_reason()
            .unwrap()
            .contains("daemon unreachable"),
        "{:?}",
        summary.not_ready_reason()
    );
}

/// A compatible but unreachable builder, with an image already installed
/// and no pending receipt, is a warning, not a failure: the installed
/// image is reused rather than the whole ready run failing.
#[tokio::test]
async fn builder_unavailable_with_an_installed_image_warns_and_reuses_it() {
    let runtime = Arc::new(FakeImportRuntime::new(true));
    let local_builder = Arc::new(FakeLocalBuilder::unavailable("daemon unreachable"));
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        ImageSources::default(),
        local_builder.clone(),
    );
    let socket = engine.session.git_root().join("docker.sock");
    engine.preparer.image_sources = local_docker_sources(&socket);

    step_through_image_phases(&mut engine, &mut frontend).await;

    let summary = engine.summary();
    match &summary.image_results[0].build {
        StepStatus::Warn(msg) => {
            assert!(msg.contains("unreachable"), "{msg}");
            assert!(msg.contains("installed image"), "{msg}");
        }
        other => panic!("expected Warn, got {other:?}"),
    }
    assert_eq!(summary.image_results[0].import, StepStatus::Done);
    // WI 0126 F1: the warning is typed on the result, not only a message.
    assert!(matches!(
        summary.image_results[0].warnings.as_slice(),
        [ReadyImageWarning::BuilderUnreachable { detail, .. }] if detail.contains("daemon unreachable")
    ));
    let imports = runtime.imports.lock().unwrap();
    assert_eq!(imports.len(), 1);
    assert!(
        !imports[0].refresh,
        "an unreachable builder with an installed image must not force refresh"
    );
    assert!(
        summary.is_ready(),
        "a build Warn (not Failed) must not block readiness"
    );
}

/// The unreachable-builder path keeps its store/reference leases through the
/// import, like every other build path, so no other awman process can
/// rebuild or retag the reference while it is being imported.
#[tokio::test]
async fn builder_unavailable_keeps_its_build_leases_through_the_import() {
    let runtime = Arc::new(FakeImportRuntime::new(true));
    let local_builder = Arc::new(FakeLocalBuilder::unavailable("daemon unreachable"));
    let (mut engine, mut frontend, _tmp, locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        ImageSources::default(),
        local_builder.clone(),
    );
    let socket = engine.session.git_root().join("docker.sock");
    engine.preparer.image_sources = local_docker_sources(&socket);
    *runtime.lock_probe_dir.lock().unwrap() = Some(locks.root().to_path_buf());

    step_through_image_phases(&mut engine, &mut frontend).await;

    assert!(matches!(
        engine.summary().image_results[0].build,
        StepStatus::Warn(_)
    ));
    let held = runtime.locks_held_at_import.lock().unwrap().clone();
    assert_eq!(held.len(), 1, "one import");
    assert!(
        held[0] >= 1,
        "the build leases must be held during the import"
    );
    assert_eq!(held_lock_files(locks.root()), 0, "and released afterwards");
}

/// A build failure (not an unreachable builder — a failed `docker
/// build`) fails only its own image's build row, skips the import, and
/// never calls the importer.
#[tokio::test]
async fn builder_build_failure_skips_the_import() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        ImageSources::default(),
        local_builder.clone(),
    );
    let socket = engine.session.git_root().join("docker.sock");
    engine.preparer.image_sources = local_docker_sources(&socket);
    let git_root = engine.session.git_root().to_path_buf();
    let project_tag = crate::data::image_tags::project_image_tag(&git_root);
    let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
    local_builder.script_inspect(&agent_tag, Ok(SourceImage::Absent));
    local_builder.script_inspect(&project_tag, Ok(SourceImage::Absent));
    local_builder.script_build(&project_tag, Err("disk full".into()));

    step_through_image_phases(&mut engine, &mut frontend).await;

    let summary = engine.summary();
    match &summary.image_results[0].build {
        StepStatus::Failed(msg) => assert!(msg.contains("disk full"), "{msg}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(summary.image_results[0].import, StepStatus::Skipped);
    assert!(runtime.imports.lock().unwrap().is_empty());
    assert!(!summary.is_ready());
}

/// An import that returns content different from what the build
/// produced is a digest mismatch, not a silently accepted substitution.
#[tokio::test]
async fn builder_import_content_mismatch_fails_closed() {
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let runtime = Arc::new(FakeImportRuntime::new(false));
    // The importer always reports digest 'f', regardless of what the
    // build receipt says it should install — simulating a retag race or
    // a broken export between the build and the import.
    runtime.set_digest_override('f');
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, _tmp, _locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        ImageSources::default(),
        local_builder.clone(),
    );
    let socket = engine.session.git_root().join("docker.sock");
    engine.preparer.image_sources = local_docker_sources(&socket);
    let git_root = engine.session.git_root().to_path_buf();
    let project_tag = crate::data::image_tags::project_image_tag(&git_root);
    let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
    script_missing_image_build(
        &local_builder,
        &project_tag,
        &agent_tag,
        built_identity('a', platform.clone()),
        built_identity('a', platform),
    );

    step_through_image_phases(&mut engine, &mut frontend).await;

    let summary = engine.summary();
    assert_eq!(summary.image_results[0].build, StepStatus::Done);
    match &summary.image_results[0].import {
        StepStatus::Failed(msg) => assert!(
            msg.to_lowercase().contains("digest") || msg.to_lowercase().contains("mismatch"),
            "{msg}"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(!summary.is_ready());
}

/// A build that succeeds followed by an import failure is reported as
/// build success + readiness failure; a later `ready` run (sharing the
/// same lock/receipt roots) must retry only the import, with forced
/// refresh, from the content the first run already verified it built —
/// never a stale cache hit — and must settle (clear) the receipt once
/// the retry succeeds.
#[tokio::test]
async fn builder_import_failure_after_a_successful_build_retries_and_settles() {
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let first_runtime = Arc::new(FakeImportRuntime::new(false));
    first_runtime.fail_next_import(1);
    let first_builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, tmp, locks, receipts) = builder_engine(
        first_runtime.clone(),
        false,
        false,
        ImageSources::default(),
        first_builder.clone(),
    );
    let socket = tmp.path().join("docker.sock");
    let sources = local_docker_sources(&socket);
    engine.preparer.image_sources = sources.clone();
    let git_root = engine.session.git_root().to_path_buf();
    let project_tag = crate::data::image_tags::project_image_tag(&git_root);
    let agent_tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
    let agent_id = built_identity('a', platform.clone());
    script_missing_image_build(
        &first_builder,
        &project_tag,
        &agent_tag,
        built_identity('a', platform.clone()),
        agent_id.clone(),
    );

    step_through_image_phases(&mut engine, &mut frontend).await;

    let summary = engine.summary();
    assert_eq!(
        summary.image_results[0].build,
        StepStatus::Done,
        "the build itself succeeded"
    );
    match &summary.image_results[0].import {
        StepStatus::Failed(msg) => assert!(msg.contains("fake import failure"), "{msg}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(!summary.is_ready());
    // WI 0126 F1/F3: build-ok/import-failed is a typed warning on the
    // result, and the hint names the retry instead of a generic line.
    assert_eq!(
        summary.image_results[0].warnings,
        [ReadyImageWarning::BuildOkImportFailed {
            tag: agent_tag.clone()
        }]
    );
    assert!(frontend
        .warning_texts()
        .iter()
        .any(|t| t == &summary.image_results[0].warnings[0].to_string()));
    assert_eq!(
        summary.hint().as_deref(),
        Some(
            "the claude image was built but not imported; run \"awman ready\" again to retry \
             the import."
        )
    );
    assert_eq!(first_runtime.imports.lock().unwrap().len(), 1);

    // A second `awman ready`, sharing the lock/receipt roots: the
    // source still holds the build's exact content (the fake reports it
    // present), and nothing needs to be rebuilt.
    let second_runtime = Arc::new(FakeImportRuntime::new(false));
    let second_builder = Arc::new(FakeLocalBuilder::new());
    second_builder.script_inspect(&agent_tag, Ok(SourceImage::Present(agent_id.clone())));
    let (mut engine2, mut frontend2) = reopen_builder_engine(
        &git_root,
        second_runtime.clone(),
        false,
        sources,
        second_builder.clone(),
        locks,
        receipts.clone(),
    );

    step_through_image_phases(&mut engine2, &mut frontend2).await;

    assert!(
        second_builder.build_calls().is_empty(),
        "the retry must not rebuild; the source already holds the receipt's content"
    );
    let imports2 = second_runtime.imports.lock().unwrap();
    assert_eq!(imports2.len(), 1);
    assert!(
        imports2[0].refresh,
        "a pending receipt must force refresh even with no --build/--refresh flag"
    );
    assert_eq!(imports2[0].expected_built_identity, Some(agent_id));
    let summary2 = engine2.summary();
    assert_eq!(summary2.image_results[0].import, StepStatus::Done);
    assert!(summary2.is_ready());

    // The receipt was cleared on the successful retry: a third run must
    // not find anything pending and must not force a refresh on its own.
    let key = PendingBuildKey {
        store: sources_default_store(&socket),
        reference: agent_tag.clone(),
        platform: crate::data::oci_identity::OciPlatform::host_linux(),
        install_tag: agent_tag.clone(),
    };
    assert!(
        receipts.load(&key).unwrap().is_none(),
        "the receipt must be cleared once the retried import is verified installed"
    );
}

/// `BuildStoreKey` for the local Docker socket `local_docker_sources`
/// configures — used only to reconstruct a `PendingBuildKey` for a
/// direct receipt-store assertion.
fn sources_default_store(socket: &std::path::Path) -> BuildStoreKey {
    BuildStoreKey::Docker {
        endpoint: format!("unix://{}", socket.display()),
    }
}

#[test]
fn build_reference_normalization_detects_docker_equivalents() {
    for reference in [
        "agent",
        "agent:latest",
        "library/agent",
        "docker.io/library/agent:latest",
        "index.docker.io/library/agent",
    ] {
        assert_eq!(
            AgentImagePreparer::normalize_build_reference(reference),
            "agent:latest"
        );
    }
    assert_eq!(
        AgentImagePreparer::normalize_build_reference("registry.example/team/agent"),
        "registry.example/team/agent:latest"
    );
}

#[test]
fn prevalidation_marks_colliding_agent_outputs_without_probing() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, _frontend, tmp, _locks, _receipts) = builder_engine(
        runtime,
        true,
        false,
        ImageSources::default(),
        builder.clone(),
    );
    write_agent_dockerfiles(tmp.path(), &["claude", "codex"]);
    let socket = tmp.path().join("docker.sock");
    for agent in ["claude", "codex"] {
        let tag = crate::data::image_tags::agent_image_tag(tmp.path(), agent);
        engine.preparer.image_sources.images.insert(
            tag,
            crate::data::config::image_source::ImageSourceSpec::DockerStore {
                host: Some(format!("unix://{}", socket.display())),
                tls: None,
                reference: Some("team/shared:latest".into()),
            },
        );
    }
    engine
        .preparer
        .prevalidate_build_targets(engine.image_policy);
    assert_eq!(engine.preparer.prevalidation_failures.len(), 2);
    assert!(engine
        .preparer
        .prevalidation_failures
        .values()
        .all(|reason| reason.contains("same local image reference")));
    assert_eq!(builder.probe_count(), 0);
}

#[tokio::test]
async fn unbound_source_with_pending_receipt_forces_fresh_import() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, tmp, _locks, receipts) = builder_engine(
        runtime,
        false,
        false,
        ImageSources::default(),
        builder.clone(),
    );
    let socket = tmp.path().join("docker.sock");
    let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    let platform = crate::data::oci_identity::OciPlatform::host_linux();
    let endpoint = crate::engine::oci::docker_engine::Endpoint::Unix(socket.clone());
    let resolved = crate::engine::oci::ResolvedImageSource {
        source: crate::data::config::image_source::ImageSourceSpec::DockerStore {
            host: Some(format!("unix://{}", socket.display())),
            tls: None,
            reference: None,
        },
        reference: tag.clone(),
        platform: platform.clone(),
        store_key: Some(crate::data::image_build_lock::BuildStoreKey::Docker {
            endpoint: endpoint.describe(),
        }),
        docker_endpoint: Some(endpoint),
    };
    let key = PendingBuildKey {
        store: sources_default_store(&socket),
        reference: AgentImagePreparer::normalize_build_reference(&tag),
        platform: platform.clone(),
        install_tag: tag.clone(),
    };
    receipts
        .save(
            &PendingBuiltImage {
                key: key.clone(),
                identity: built_identity('a', platform),
            },
            || Ok(()),
        )
        .unwrap();
    builder.script_inspect(&tag, Ok(SourceImage::Unbound));
    let selected = BuilderSelection::from_source(Some(&resolved), true);
    let prepared = engine
        .preparer
        .prepare_local_image(
            &mut frontend,
            &resolved,
            &selected,
            &AgentImageRequest {
                tag: &tag,
                agent: "claude",
                policy: engine.image_policy,
            },
        )
        .await
        .unwrap();
    assert!(prepared.force_refresh);
    assert!(prepared.expected.is_none());
    assert_eq!(prepared.pending_key, Some(key.clone()));
    assert!(receipts.load(&key).unwrap().is_none());
}

#[tokio::test]
async fn failed_prevalidation_closes_the_cached_image_progress_row() {
    let runtime = Arc::new(FakeImportRuntime::new(true));
    let builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, tmp, _locks, _receipts) =
        builder_engine(runtime, false, false, ImageSources::default(), builder);
    let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    engine.preparer.image_sources = local_docker_sources(&tmp.path().join("docker.sock"));
    engine
        .preparer
        .prevalidate_build_targets(engine.image_policy);
    engine
        .preparer
        .prevalidation_failures
        .insert(tag.clone(), "collision".into());
    assert!(engine
        .prepare_agent_image(&mut frontend, tmp.path(), "claude")
        .await
        .is_err());
    let cached = frontend
        .statuses
        .iter()
        .filter(|(step, _)| step.contains("cached from"));
    let statuses: Vec<_> = cached.map(|(_, status)| status).collect();
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0], &StepStatus::Running);
    assert_eq!(statuses[1], &StepStatus::Failed("collision".into()));
}

/// A check-only launch (`chat`, `exec`) that has to wait for another awman
/// process's build lease says so, instead of blocking silently.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_another_processs_build_lease_is_reported() {
    let runtime = Arc::new(FakeImportRuntime::new(true));
    let local_builder = Arc::new(FakeLocalBuilder::new());
    let (mut engine, mut frontend, _tmp, locks, _receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        ImageSources::default(),
        local_builder,
    );
    let git_root = engine.session.git_root().to_path_buf();
    let socket = git_root.join("docker.sock");
    engine.preparer.image_sources = local_docker_sources(&socket);
    let tag = crate::data::image_tags::agent_image_tag(&git_root, "claude");
    let plan = engine.preparer.plan(&tag, "claude");
    let source = plan.source.expect("local docker source resolves");
    let store = source.store_key.clone().expect("a local store");
    let reference = AgentImagePreparer::normalize_build_reference(&source.reference);

    // "Another process": a lease held for a moment on another thread.
    let held = locks.lock(&store, &reference, || Ok(())).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(held);
    });
    let _ = engine
        .preparer
        .prepare(
            &AgentImageRequest {
                tag: &tag,
                agent: "claude",
                policy: BuildPolicy::CheckOnly,
            },
            &mut frontend,
        )
        .await;
    release.join().unwrap();
    assert!(
        frontend
            .messages
            .iter()
            .any(|m| m.text.contains("Waiting for another awman process")),
        "{:?}",
        frontend.messages
    );
}
