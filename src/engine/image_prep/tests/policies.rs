use super::*;
use crate::data::config::image_source::{ImageSourceKind, ImageSourceSpec};
use crate::data::oci_identity::OciPlatform;

const READY: BuildPolicy = BuildPolicy::Ready {
    build: false,
    no_cache: false,
    refresh: false,
};

#[tokio::test]
async fn check_only_refuses_pending_build_and_changed_or_invalid_source() {
    for scenario in ["pending", "changed", "invalid"] {
        let runtime = Arc::new(FakeImportRuntime::new(true));
        let builder = Arc::new(FakeLocalBuilder::new());
        let (mut run, mut sink, tmp, _, receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            local_docker_sources(std::path::Path::new("/test/docker.sock")),
            builder.clone(),
        );
        let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
        let frozen = run.preparer.plan(&tag, "claude").source.unwrap();
        *runtime.cached_request.lock().unwrap() =
            Some(crate::engine::agent_runtime::ImageImportRequest {
                tag: tag.clone(),
                source: frozen.source,
                platform: frozen.platform,
                refresh: false,
                expected_built_identity: None,
                immutable_export_selector: None,
                registries: Default::default(),
            });
        if scenario == "pending" {
            let key = PendingBuildKey {
                store: BuildStoreKey::Docker {
                    endpoint: "unix:///test/docker.sock".into(),
                },
                reference: AgentImagePreparer::normalize_build_reference(&tag),
                platform: OciPlatform::host_linux(),
                install_tag: tag.clone(),
            };
            receipts
                .save(
                    &PendingBuiltImage {
                        key,
                        identity: built_identity('c', OciPlatform::host_linux()),
                    },
                    || Ok(()),
                )
                .unwrap();
        } else {
            run.preparer.plans.clear();
            run.preparer.image_sources.default = Some(ImageSourceSpec::DockerStore {
                host: Some(
                    if scenario == "invalid" {
                        "tcp://build.example:2375"
                    } else {
                        "unix:///other/docker.sock"
                    }
                    .into(),
                ),
                tls: None,
                reference: Some("other/agent:latest".into()),
            });
        }
        let result = run
            .preparer
            .prepare(
                &AgentImageRequest {
                    tag: &tag,
                    agent: "claude",
                    policy: BuildPolicy::CheckOnly,
                },
                &mut sink,
            )
            .await;
        assert!(result.is_err(), "{scenario} must require ready");
        assert!(runtime.imports.lock().unwrap().is_empty());
        assert_eq!(builder.probe_count(), 0);
        assert_eq!(builder.inspect_count(), 0);
        assert!(builder.build_calls().is_empty());
    }
}

#[test]
fn plans_cache_the_agent_as_well_as_the_tag() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut run, _, tmp) = import_engine(runtime, false, archive_sources());
    let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    assert!(run.preparer.plan(&tag, "claude").pinned_default.is_none());
    assert!(run.preparer.plan(&tag, "codex").pinned_default.is_some());
    assert!(run.preparer.plan(&tag, "claude").pinned_default.is_none());
}

#[tokio::test]
async fn disabled_builder_ignores_stale_receipts_and_build_locks() {
    let mut policies = vec![BuildPolicy::SessionSetup, BuildPolicy::ImportOnly];
    for build in [false, true] {
        for no_cache in [false, true] {
            for refresh in [false, true] {
                policies.push(BuildPolicy::Ready {
                    build,
                    no_cache,
                    refresh,
                });
            }
        }
    }
    for policy in policies {
        let runtime = Arc::new(FakeImportRuntime::new(true));
        runtime.set_digest_override('b');
        let builder = Arc::new(FakeLocalBuilder::new());
        let (mut run, mut sink, tmp, locks, receipts) = builder_engine(
            runtime.clone(),
            false,
            false,
            local_docker_sources(std::path::Path::new("/test/docker.sock")),
            builder.clone(),
        );
        run.preparer.builder_enabled = false;
        let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
        let key = PendingBuildKey {
            store: BuildStoreKey::Docker {
                endpoint: "unix:///test/docker.sock".into(),
            },
            reference: AgentImagePreparer::normalize_build_reference(&tag),
            platform: OciPlatform::host_linux(),
            install_tag: tag.clone(),
        };
        let receipt = PendingBuiltImage {
            key: key.clone(),
            identity: built_identity('c', OciPlatform::host_linux()),
        };
        receipts.save(&receipt, || Ok(())).unwrap();
        let request = AgentImageRequest {
            tag: &tag,
            agent: "claude",
            policy,
        };
        let result = run.preparer.prepare(&request, &mut sink).await.unwrap();
        assert_eq!(result.import, StepStatus::Done, "{policy:?}");
        assert_eq!(result.build, StepStatus::Skipped);
        // WI 0125 disabled mode does not read or mutate build receipts.
        assert_eq!(
            receipts.load(&key).unwrap().unwrap().identity,
            receipt.identity
        );
        let lease = locks.lock(&key.store, &key.reference, || Ok(())).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            run.preparer.prepare(&request, &mut sink),
        )
        .await
        .expect("disabled imports must not wait for build leases")
        .unwrap();
        assert_eq!(result.import, StepStatus::Done);
        drop(lease);
        for imported in runtime.imports.lock().unwrap().iter() {
            assert_eq!(imported.refresh, policy.refresh_import());
            assert_eq!(imported.expected_built_identity, None);
            assert_eq!(imported.immutable_export_selector, None);
        }
        assert_eq!(builder.probe_count(), 0);
        assert_eq!(builder.inspect_count(), 0);
        assert!(builder.build_calls().is_empty());
        assert!(sink.warning_texts().is_empty());
    }
}

#[tokio::test]
async fn endpoint_resolution_errors_preserve_stages_without_provisioning() {
    for enabled in [false, true] {
        for policy in [READY, BuildPolicy::SessionSetup, BuildPolicy::ImportOnly] {
            let runtime = Arc::new(FakeImportRuntime::new(false));
            let builder = Arc::new(FakeLocalBuilder::new());
            let (mut run, mut sink, tmp, _, _) = builder_engine(
                runtime.clone(),
                false,
                false,
                local_docker_sources(std::path::Path::new("/test/docker.sock")),
                builder.clone(),
            );
            run.preparer.builder_enabled = enabled;
            run.preparer.image_sources.default = Some(ImageSourceSpec::DockerStore {
                host: Some("tcp://build.example:2375".into()),
                tls: None,
                reference: None,
            });
            let dockerfile = tmp.path().join(".awman/Dockerfile.claude");
            std::fs::remove_file(&dockerfile).unwrap();
            let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
            let result = run
                .preparer
                .prepare(
                    &AgentImageRequest {
                        tag: &tag,
                        agent: "claude",
                        policy,
                    },
                    &mut sink,
                )
                .await
                .unwrap();
            if enabled && policy.allows_build() {
                assert!(
                    matches!(result.build, StepStatus::Failed(ref text) if text.contains("tls")),
                    "{result:?}"
                );
                assert_eq!(result.import, StepStatus::Skipped);
            } else {
                assert_eq!(result.build, StepStatus::Skipped);
                assert!(
                    matches!(result.import, StepStatus::Failed(ref text) if text.contains("tls")),
                    "{result:?}"
                );
            }
            assert_eq!(result.source_kind, Some(ImageSourceKind::DockerStore));
            assert_eq!(result.reference.as_deref(), Some(tag.as_str()));
            assert!(
                !dockerfile.exists(),
                "a configuration error must not download templates"
            );
            assert!(sink.warning_texts().is_empty());
            assert!(runtime.imports.lock().unwrap().is_empty());
            assert_eq!(builder.probe_count(), 0);
            assert_eq!(builder.inspect_count(), 0);
            assert!(builder.build_calls().is_empty());
        }
    }
}

#[test]
fn policies_preserve_build_and_refresh_semantics() {
    for build in [false, true] {
        for no_cache in [false, true] {
            for refresh in [false, true] {
                let policy = BuildPolicy::Ready {
                    build,
                    no_cache,
                    refresh,
                };
                assert!(policy.allows_build());
                assert_eq!(policy.force_build(), build);
                assert_eq!(policy.no_cache(), no_cache);
                assert_eq!(policy.refresh_import(), build || no_cache);
                assert!(policy.audit_rebuild().force_build());
            }
        }
    }
    assert!(BuildPolicy::SessionSetup.allows_build());
    assert!(!BuildPolicy::SessionSetup.force_build());
    assert!(!BuildPolicy::SessionSetup.audit_rebuild().force_build());
    for policy in [BuildPolicy::ImportOnly, BuildPolicy::CheckOnly] {
        assert!(!policy.allows_build());
        assert!(!policy.refresh_import());
        assert_eq!(policy.audit_rebuild(), policy);
    }
}

#[tokio::test]
async fn preflight_and_ready_acquire_the_same_frozen_source_with_builder_on_or_off() {
    let sources = [
        ImageSourceSpec::DockerStore {
            host: Some("unix:///test/docker.sock".into()),
            tls: None,
            reference: Some("team/agent:1".into()),
        },
        ImageSourceSpec::AppleStore {
            reference: Some("team/agent:1".into()),
        },
        ImageSourceSpec::DockerStore {
            host: Some("tcp://127.0.0.1:2375".into()),
            tls: None,
            reference: Some("team/agent:1".into()),
        },
        ImageSourceSpec::Archive {
            path: "/export/agent.tar".into(),
        },
        ImageSourceSpec::Registry {
            registry: Some("registry.example".into()),
            reference: Some("team/agent:1".into()),
        },
    ];
    for enabled in [false, true] {
        for source in &sources {
            let runtime = Arc::new(FakeImportRuntime::new(true));
            let builder = Arc::new(FakeLocalBuilder::new());
            let sources = ImageSources {
                default: Some(source.clone()),
                ..Default::default()
            };
            let (mut ready, mut sink, tmp, locks, receipts) = builder_engine(
                runtime.clone(),
                false,
                false,
                sources.clone(),
                builder.clone(),
            );
            ready.preparer.builder_enabled = enabled;
            let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
            let platform = OciPlatform::host_linux();
            // A source store containing the same image should need no build.
            builder.script_inspect(
                &source.reference_or(&tag),
                Ok(SourceImage::Present(built_identity('a', platform))),
            );
            let result = ready
                .preparer
                .prepare(
                    &AgentImageRequest {
                        tag: &tag,
                        agent: "claude",
                        policy: READY,
                    },
                    &mut sink,
                )
                .await
                .unwrap();
            assert_eq!(result.import, StepStatus::Done);
            let ready_request = runtime.imports.lock().unwrap()[0].clone();
            let preflight_runtime = Arc::new(FakeImportRuntime::new(true));
            let preflight_builder = Arc::new(FakeLocalBuilder::new());
            let (mut preflight, mut sink) = reopen_builder_engine(
                tmp.path(),
                preflight_runtime.clone(),
                false,
                sources,
                preflight_builder.clone(),
                locks,
                receipts,
            );
            preflight.preparer.builder_enabled = enabled;
            let result = preflight
                .preparer
                .prepare(
                    &AgentImageRequest {
                        tag: &tag,
                        agent: "claude",
                        policy: BuildPolicy::ImportOnly,
                    },
                    &mut sink,
                )
                .await
                .unwrap();
            assert_eq!(result.import, StepStatus::Done);
            assert_eq!(result.build, StepStatus::Skipped);
            assert!(result.warnings.is_empty());
            assert!(sink.warning_texts().is_empty());
            let requests = preflight_runtime.imports.lock().unwrap();
            assert_eq!(
                requests.len(),
                1,
                "preflight must recheck provenance even for installed images"
            );
            assert_eq!(requests[0].tag, ready_request.tag);
            assert_eq!(requests[0].source, ready_request.source);
            assert_eq!(requests[0].platform, ready_request.platform);
            assert_eq!(requests[0].registries, ready_request.registries);
            assert_eq!(preflight_builder.probe_count(), 0);
            assert_eq!(preflight_builder.inspect_count(), 0);
            assert!(preflight_builder.build_calls().is_empty());
        }
    }
}

#[tokio::test]
async fn import_only_and_check_only_never_construct_a_host_builder_or_warn_about_building() {
    for policy in [BuildPolicy::ImportOnly, BuildPolicy::CheckOnly] {
        for present in [false, true] {
            for sources in [
                archive_sources(),
                local_docker_sources(std::path::Path::new("/test/docker.sock")),
            ] {
                let runtime = Arc::new(FakeImportRuntime::new(present));
                let (mut run, mut sink, tmp, _, _) = builder_engine(
                    runtime.clone(),
                    true,
                    true,
                    sources,
                    Arc::new(FakeLocalBuilder::new()),
                );
                run.preparer.local_builder = None;
                let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
                let frozen = run.preparer.plan(&tag, "claude").source.unwrap();
                *runtime.cached_request.lock().unwrap() =
                    Some(crate::engine::agent_runtime::ImageImportRequest {
                        tag: tag.clone(),
                        source: frozen.source,
                        platform: frozen.platform,
                        refresh: false,
                        expected_built_identity: None,
                        immutable_export_selector: None,
                        registries: Default::default(),
                    });
                let result = run
                    .preparer
                    .prepare(
                        &AgentImageRequest {
                            tag: &tag,
                            agent: "claude",
                            policy,
                        },
                        &mut sink,
                    )
                    .await;
                assert!(run.preparer.local_builder.is_none());
                assert!(sink.warning_texts().is_empty());
                assert_eq!(
                    runtime.imports.lock().unwrap().len(),
                    usize::from(policy == BuildPolicy::ImportOnly)
                );
                if policy == BuildPolicy::CheckOnly && !present {
                    assert!(matches!(
                        result.unwrap_err(),
                        EngineError::BuiltinImageNotImported { image } if image == tag
                    ));
                } else {
                    let result = result.unwrap();
                    assert_eq!(result.build, StepStatus::Skipped);
                    assert_eq!(result.import, StepStatus::Done);
                    assert!(result.warnings.is_empty());
                }
            }
        }
    }
}

#[tokio::test]
async fn session_setup_builds_only_missing_source_images_and_refreshes_after_build() {
    for source_present in [false, true] {
        let runtime = Arc::new(FakeImportRuntime::new(true));
        let builder = Arc::new(FakeLocalBuilder::new());
        let (mut run, mut sink, tmp, _, _) = builder_engine(
            runtime.clone(),
            true,
            true,
            local_docker_sources(std::path::Path::new("/test/docker.sock")),
            builder.clone(),
        );
        let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
        let project_tag = crate::data::image_tags::project_image_tag(tmp.path());
        if source_present {
            builder.script_inspect(
                &tag,
                Ok(SourceImage::Present(built_identity(
                    'b',
                    OciPlatform::host_linux(),
                ))),
            );
        } else {
            script_missing_image_build(
                &builder,
                &project_tag,
                &tag,
                built_identity('a', OciPlatform::host_linux()),
                built_identity('b', OciPlatform::host_linux()),
            );
        }
        let result = run
            .preparer
            .prepare(
                &AgentImageRequest {
                    tag: &tag,
                    agent: "claude",
                    policy: BuildPolicy::SessionSetup,
                },
                &mut sink,
            )
            .await
            .unwrap();
        assert_eq!(result.import, StepStatus::Done);
        assert_eq!(
            builder.build_calls().len(),
            if source_present { 0 } else { 2 }
        );
        assert!(builder
            .build_calls()
            .iter()
            .all(|request| !request.no_cache));
        assert_eq!(
            result.build,
            if source_present {
                StepStatus::Skipped
            } else {
                StepStatus::Done
            }
        );
        assert_eq!(runtime.imports.lock().unwrap()[0].refresh, !source_present);
    }
}

#[tokio::test]
async fn disabled_builder_preserves_build_no_cache_and_refresh_acquisition_flags() {
    for build in [false, true] {
        for no_cache in [false, true] {
            for refresh in [false, true] {
                let runtime = Arc::new(FakeImportRuntime::new(true));
                let (mut run, mut sink, tmp) =
                    import_engine(runtime.clone(), build, archive_sources());
                let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
                let result = run
                    .preparer
                    .prepare(
                        &AgentImageRequest {
                            tag: &tag,
                            agent: "claude",
                            policy: BuildPolicy::Ready {
                                build,
                                no_cache,
                                refresh,
                            },
                        },
                        &mut sink,
                    )
                    .await
                    .unwrap();
                assert_eq!(result.build, StepStatus::Skipped);
                assert_eq!(result.import, StepStatus::Done);
                assert!(run.preparer.local_builder.is_none());
                assert_eq!(
                    runtime.imports.lock().unwrap()[0].refresh,
                    build || no_cache
                );
            }
        }
    }
}

#[tokio::test]
async fn preflight_cannot_accept_stale_image_when_build_receipt_is_pending() {
    let runtime = Arc::new(FakeImportRuntime::new(true));
    runtime.set_digest_override('b');
    let builder = Arc::new(FakeLocalBuilder::new());
    let socket = std::path::Path::new("/test/docker.sock");
    let (mut run, mut sink, tmp, _, receipts) = builder_engine(
        runtime.clone(),
        false,
        false,
        local_docker_sources(socket),
        builder.clone(),
    );
    let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    let key = PendingBuildKey {
        store: BuildStoreKey::Docker {
            endpoint: "unix:///test/docker.sock".into(),
        },
        reference: AgentImagePreparer::normalize_build_reference(&tag),
        platform: OciPlatform::host_linux(),
        install_tag: tag.clone(),
    };
    let expected = built_identity('c', OciPlatform::host_linux());
    receipts
        .save(
            &PendingBuiltImage {
                key: key.clone(),
                identity: expected.clone(),
            },
            || Ok(()),
        )
        .unwrap();
    let request = AgentImageRequest {
        tag: &tag,
        agent: "claude",
        policy: BuildPolicy::ImportOnly,
    };
    let failed = run.preparer.prepare(&request, &mut sink).await.unwrap();
    assert!(matches!(failed.import, StepStatus::Failed(_)));
    assert_eq!(failed.build, StepStatus::Skipped);
    assert!(receipts.load(&key).unwrap().is_some());
    let imported = runtime.imports.lock().unwrap()[0].clone();
    assert!(imported.refresh);
    assert_eq!(imported.expected_built_identity, Some(expected.clone()));
    assert_eq!(
        imported.immutable_export_selector.as_deref(),
        expected.config_digest().map(|d| d.as_str())
    );
    *runtime.digest_override.lock().unwrap() = None;
    assert_eq!(
        run.preparer
            .prepare(&request, &mut sink)
            .await
            .unwrap()
            .import,
        StepStatus::Done
    );
    assert!(receipts.load(&key).unwrap().is_none());
    assert_eq!(builder.probe_count(), 0);
    assert_eq!(builder.inspect_count(), 0);
    assert!(builder.build_calls().is_empty());
    assert!(sink.warning_texts().is_empty());
}

#[tokio::test]
async fn source_and_selection_are_frozen_for_the_run_including_audit_rebuild() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let builder = Arc::new(FakeLocalBuilder::new());
    let (mut run, mut sink, tmp, _, _) = builder_engine(
        runtime.clone(),
        false,
        false,
        archive_sources(),
        builder.clone(),
    );
    let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    let first = run
        .preparer
        .prepare(
            &AgentImageRequest {
                tag: &tag,
                agent: "claude",
                policy: READY,
            },
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(first.warnings.len(), 1);
    let original = runtime.imports.lock().unwrap()[0].source.clone();
    run.preparer.image_sources = local_docker_sources(std::path::Path::new("/changed.sock"));
    run.preparer.builder_enabled = false;
    run.preparer.reset_bases();
    let second = run
        .preparer
        .prepare(
            &AgentImageRequest {
                tag: &tag,
                agent: "claude",
                policy: READY.audit_rebuild(),
            },
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(second.import, StepStatus::Done);
    assert!(second.warnings.is_empty());
    assert_eq!(sink.warning_texts().len(), 1);
    assert_eq!(runtime.imports.lock().unwrap()[1].source, original);
    assert_eq!(builder.inspect_count(), 0);
    assert!(builder.build_calls().is_empty());
}

#[tokio::test]
async fn mixed_sources_build_and_import_each_agents_own_reference() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let builder = Arc::new(FakeLocalBuilder::new());
    let (mut run, mut sink, tmp, _, _) = builder_engine(
        runtime.clone(),
        true,
        false,
        local_docker_sources(std::path::Path::new("/test/docker.sock")),
        builder.clone(),
    );
    write_agent_dockerfiles(tmp.path(), &["claude", "codex"]);
    let claude_tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    let codex_tag = crate::data::image_tags::agent_image_tag(tmp.path(), "codex");
    let project_tag = crate::data::image_tags::project_image_tag(tmp.path());
    let docker = ImageSourceSpec::DockerStore {
        host: Some("unix:///test/docker.sock".into()),
        tls: None,
        reference: Some("team/claude:build".into()),
    };
    let archive = ImageSourceSpec::Archive {
        path: "/export/codex.tar".into(),
    };
    run.preparer
        .image_sources
        .images
        .insert(claude_tag.clone(), docker.clone());
    run.preparer
        .image_sources
        .images
        .insert(codex_tag.clone(), archive.clone());
    let base = built_identity('a', OciPlatform::host_linux());
    builder.script_build(&project_tag, Ok(base.clone()));
    builder.script_inspect(&project_tag, Ok(SourceImage::Present(base)));
    builder.script_build(
        "team/claude:build",
        Ok(built_identity('c', OciPlatform::host_linux())),
    );
    step_through_image_phases(&mut run, &mut sink).await;
    assert!(run.summary().is_ready());
    let builds = builder.build_calls();
    assert_eq!(
        builds
            .iter()
            .map(|build| build.reference.as_str())
            .collect::<Vec<_>>(),
        vec![project_tag.as_str(), "team/claude:build"]
    );
    let imports = runtime.imports.lock().unwrap();
    assert_eq!(imports.len(), 2);
    assert_eq!(
        (&imports[0].tag, &imports[0].source),
        (&claude_tag, &docker)
    );
    assert_eq!(
        (&imports[1].tag, &imports[1].source),
        (&codex_tag, &archive)
    );
    assert_eq!(
        sink.warning_texts()
            .iter()
            .filter(|text| text.contains("cannot build Dockerfiles"))
            .count(),
        1
    );
}

#[tokio::test]
async fn check_only_uses_the_configured_project_dockerfile_in_its_hint() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut run, mut sink, tmp) = import_engine(runtime.clone(), false, ImageSources::default());
    run.preparer.context = ImageBuildContext::for_repo(
        tmp.path(),
        &crate::data::config::RepoConfig {
            dockerfile: Some("containers/Project.Dockerfile".into()),
            ..Default::default()
        },
        "claude",
    );
    let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    let error = run
        .preparer
        .prepare(
            &AgentImageRequest {
                tag: &tag,
                agent: "claude",
                policy: BuildPolicy::CheckOnly,
            },
            &mut sink,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("containers/Project.Dockerfile"));
    assert!(!error.to_string().contains("Dockerfile.dev"));
    assert!(runtime.imports.lock().unwrap().is_empty());
}

#[tokio::test]
async fn import_only_missing_source_returns_hint_without_a_build_warning() {
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let (mut run, mut sink, tmp) = import_engine(runtime.clone(), false, ImageSources::default());
    let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    let result = run
        .preparer
        .prepare(
            &AgentImageRequest {
                tag: &tag,
                agent: "claude",
                policy: BuildPolicy::ImportOnly,
            },
            &mut sink,
        )
        .await
        .unwrap();
    assert!(
        matches!(result.import, StepStatus::Failed(ref text) if text.contains("no image source is configured"))
    );
    assert!(sink.warning_texts().is_empty());
    assert!(runtime.imports.lock().unwrap().is_empty());
    assert!(run.preparer.local_builder.is_none());
}

#[tokio::test]
async fn preparation_holds_base_and_agent_leases_until_import_finishes() {
    use crate::data::message::UserMessageSink;
    use crate::engine::agent_runtime::{ImageImportRequest, ImportedImage};
    use crate::engine::oci::CancelToken;
    struct PausedImport {
        started: Arc<tokio::sync::Notify>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        runtime: Arc<FakeImportRuntime>,
    }
    impl ImageImporter for PausedImport {
        fn import(
            &self,
            request: &ImageImportRequest,
            _: &CancelToken,
            sink: &mut dyn UserMessageSink,
        ) -> Result<ImportedImage, EngineError> {
            self.started.notify_one();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            self.runtime.import_image(request, sink)
        }
    }
    let runtime = Arc::new(FakeImportRuntime::new(false));
    let builder = Arc::new(FakeLocalBuilder::new());
    let (mut run, mut sink, tmp, locks, _) = builder_engine(
        runtime.clone(),
        false,
        false,
        local_docker_sources(std::path::Path::new("/test/docker.sock")),
        builder.clone(),
    );
    let tag = crate::data::image_tags::agent_image_tag(tmp.path(), "claude");
    let base = crate::data::image_tags::project_image_tag(tmp.path());
    script_missing_image_build(
        &builder,
        &base,
        &tag,
        built_identity('a', OciPlatform::host_linux()),
        built_identity('b', OciPlatform::host_linux()),
    );
    let started = Arc::new(tokio::sync::Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    run.preparer = run.preparer.with_image_importer(Arc::new(PausedImport {
        started: started.clone(),
        release: std::sync::Mutex::new(wait),
        runtime,
    }));
    let agent_tag = tag.clone();
    let preparation = tokio::spawn(async move {
        run.preparer
            .prepare(
                &AgentImageRequest {
                    tag: &agent_tag,
                    agent: "claude",
                    policy: READY,
                },
                &mut sink,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let store = BuildStoreKey::Docker {
        endpoint: "unix:///test/docker.sock".into(),
    };
    for reference in [&base, &tag] {
        let locks = locks.clone();
        let store = store.clone();
        let reference = AgentImagePreparer::normalize_build_reference(reference);
        let blocked = tokio::task::spawn_blocking(move || {
            let mut checks = 0;
            locks
                .lock(&store, &reference, || {
                    checks += 1;
                    if checks > 1 {
                        Err(crate::data::error::DataError::Other("still locked".into()))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
        })
        .await
        .unwrap();
        assert!(
            blocked,
            "the reference must remain locked through the import"
        );
    }
    release.send(()).unwrap();
    assert_eq!(preparation.await.unwrap().unwrap().import, StepStatus::Done);
    for reference in [&base, &tag] {
        assert!(locks
            .lock(
                &store,
                &AgentImagePreparer::normalize_build_reference(reference),
                || Ok(())
            )
            .is_ok());
    }
}
