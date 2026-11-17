use super::*;
use crate::data::{
    config::image_source::ImageSourceSpec,
    image_tags::{agent_image_tag, project_image_tag},
};
use crate::engine::image_prep::contracts::{identity, preparer, Sink, Store};
use awman_test_support::{TestCase, TestSandbox};
use std::sync::{atomic::Ordering, Arc, Mutex};

fn local() -> Option<ImageSourceSpec> {
    Some(ImageSourceSpec::AppleStore { reference: None })
}
#[derive(Default)]
struct Output {
    begins: Vec<String>,
    lines: Vec<String>,
    finishes: Vec<(String, Option<String>)>,
}
impl BuildOutputTarget for Output {
    fn begin(&mut self, tag: &str) {
        self.begins.push(tag.into());
    }
    fn line(&mut self, line: &str) {
        self.lines.push(line.into());
    }
    fn finish(&mut self, tag: &str, error: Option<&str>) {
        self.finishes.push((tag.into(), error.map(str::to_string)));
    }
}

#[tokio::test]
async fn local_builder_preflight_refreshes_inputs_and_preserves_installed_image_on_failure() {
    let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::local_builder_preflight_refreshes_inputs_and_preserves_installed_image_on_failure"));
    let store = Arc::new(Store::default());
    let tag = agent_image_tag(&s.work(), "claude");
    #[derive(Default)]
    struct Observed {
        messages: Vec<String>,
        warnings: Vec<crate::data::execution_event::ImageStaleWarning>,
    }
    impl UserMessageSink for Observed {
        fn write_message(&mut self, message: UserMessage) {
            self.messages.push(message.text);
        }
        fn replay_queued(&mut self) {}
        fn report_image_stale(
            &mut self,
            warning: &crate::data::execution_event::ImageStaleWarning,
        ) {
            self.warnings.push(warning.clone());
        }
    }
    let mut sink = Observed::default();
    let prep = preparer(&s, store.clone(), true, local());
    prepare_importing_step_with_recovery(prep, &tag, "claude", &mut sink, None)
        .await
        .unwrap();
    assert_eq!(store.built.lock().unwrap().len(), 2);
    let prep = preparer(&s, store.clone(), true, local());
    std::fs::write(
        s.work().join("Dockerfile.dev"),
        "FROM scratch\nRUN echo changed\n",
    )
    .unwrap();
    prepare_importing_step_with_recovery(prep, &tag, "claude", &mut sink, None)
        .await
        .unwrap();
    assert_eq!(store.built.lock().unwrap().len(), 4);
    let prep = preparer(&s, store.clone(), true, local());
    std::fs::write(
        s.work().join("Dockerfile.dev"),
        "FROM scratch\nRUN echo changed\n",
    )
    .unwrap();
    prepare_importing_step_with_recovery(prep, &tag, "claude", &mut sink, None)
        .await
        .unwrap();
    assert_eq!(
        store.built.lock().unwrap().len(),
        4,
        "same source inputs converge"
    );
    let installed = store.installed.lock().unwrap()[&tag].clone();
    let base = project_image_tag(&s.work());
    let installed_inputs = store.source_metadata.lock().unwrap()[&base].labels
        [crate::data::image_inputs::INPUTS_LABEL]
        .clone();
    store.fail_build.store(true, Ordering::SeqCst);
    let prep = preparer(&s, store.clone(), true, local());
    std::fs::write(s.work().join("Dockerfile.dev"), "FROM scratch\nRUN false\n").unwrap();
    let expected = crate::data::image_inputs::project_inputs(
        Some(b"FROM scratch\nRUN false\n"),
        &crate::data::image_inputs::DirBuildContext::new(s.work()),
    )
    .hash;
    prepare_importing_step_with_recovery(prep, &tag, "claude", &mut sink, None)
        .await
        .unwrap();
    assert_eq!(store.installed.lock().unwrap()[&tag], installed);
    assert_eq!(sink.warnings.len(), 1);
    assert_eq!(
        sink.warnings[0].kind,
        crate::data::execution_event::StaleImageKind::Project
    );
    assert_eq!(sink.warnings[0].image, base);
    assert_eq!(sink.warnings[0].expected_inputs, expected.as_str());
    assert_eq!(
        sink.warnings[0].installed_inputs.as_deref(),
        Some(installed_inputs.as_str())
    );
    s.finish();
}

#[tokio::test]
async fn missing_image_preflight_builds_once_routes_output_and_reuses_source_images() {
    let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::missing_image_preflight_builds_once_routes_output_and_reuses_source_images"));
    let store = Arc::new(Store::default());
    let tag = agent_image_tag(&s.work(), "claude");
    let base = project_image_tag(&s.work());
    let mut sink = Sink::default();
    let mut output = Output::default();
    prepare_importing_step_with_recovery(
        preparer(&s, store.clone(), true, local()),
        &tag,
        "claude",
        &mut sink,
        Some(&mut output),
    )
    .await
    .unwrap();
    assert_eq!(*store.built.lock().unwrap(), [base.clone(), tag.clone()]);
    assert_eq!(output.begins, [base, tag.clone()]);
    assert_eq!(output.lines, ["builder stdout", "builder stdout"]);
    assert!(output.finishes.iter().all(|(_, error)| error.is_none()));
    assert!(!sink.0.iter().any(|line| line == "builder stdout"));
    assert!(sink.0.iter().any(|line| line.contains("Building image")));
    // Installed image, then missing runtime image but present source image:
    // neither is a reason to rebuild the source store.
    for installed in [true, false] {
        if !installed {
            store.installed.lock().unwrap().clear();
        }
        prepare_importing_step_with_recovery(
            preparer(&s, store.clone(), true, local()),
            &tag,
            "claude",
            &mut sink,
            None,
        )
        .await
        .unwrap();
    }
    assert_eq!(store.built.lock().unwrap().len(), 2);
    s.finish();
}

#[tokio::test]
async fn failed_build_keeps_diagnostics_and_never_imports_and_apple_recovery_still_builds() {
    let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::failed_build_keeps_diagnostics_and_never_imports_and_apple_recovery_still_builds"));
    let store = Arc::new(Store::default());
    let tag = agent_image_tag(&s.work(), "claude");
    store.fail_build.store(true, Ordering::SeqCst);
    let mut sink = Sink::default();
    let mut output = Output::default();
    let error = prepare_importing_step_with_recovery(
        preparer(&s, store.clone(), true, local()),
        &tag,
        "claude",
        &mut sink,
        Some(&mut output),
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("builder stderr: invalid Dockerfile"));
    assert!(output.finishes[0]
        .1
        .as_ref()
        .unwrap()
        .contains("invalid Dockerfile"));
    assert!(store.imports.lock().unwrap().is_empty());
    store.fail_build.store(false, Ordering::SeqCst);
    store
        .installed
        .lock()
        .unwrap()
        .insert(tag.clone(), identity(&tag));
    store.apple_missing.store(true, Ordering::SeqCst);
    prepare_importing_step_with_recovery(
        preparer(&s, store.clone(), true, local()),
        &tag,
        "claude",
        &mut sink,
        None,
    )
    .await
    .unwrap();
    assert_eq!(store.imports.lock().unwrap().len(), 2);
    assert!(sink
        .0
        .iter()
        .any(|line| line.contains("Apple Containers is missing content")));
    assert!(sink.0.iter().any(|line| line == "builder stdout"));
    s.finish();
}

#[tokio::test]
async fn disabled_and_incompatible_sources_do_not_build_or_emit_build_warnings() {
    let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::disabled_and_incompatible_sources_do_not_build_or_emit_build_warnings"));
    let tag = agent_image_tag(&s.work(), "claude");
    for (enabled, source) in [
        (false, local()),
        (
            true,
            Some(ImageSourceSpec::Registry {
                registry: None,
                reference: None,
            }),
        ),
        (
            true,
            Some(ImageSourceSpec::Archive {
                path: s.work().join("image.tar"),
            }),
        ),
        (
            true,
            Some(ImageSourceSpec::DockerStore {
                host: Some("tcp://127.0.0.1:2375".into()),
                tls: None,
                reference: None,
            }),
        ),
    ] {
        let store = Arc::new(Store::default());
        store.fail_build.store(true, Ordering::SeqCst);
        let mut sink = Sink::default();
        prepare_importing_step_with_recovery(
            preparer(&s, store.clone(), enabled, source),
            &tag,
            "claude",
            &mut sink,
            None,
        )
        .await
        .unwrap();
        assert!(store.built.lock().unwrap().is_empty());
        assert_eq!(store.imports.lock().unwrap().len(), 1);
        assert!(!sink
            .0
            .iter()
            .any(|line| line.contains("cannot build Dockerfiles")));
    }
    s.finish();
}

struct WaitingSink {
    messages: Arc<Mutex<Vec<String>>>,
    waiting: Arc<tokio::sync::Notify>,
}
impl UserMessageSink for WaitingSink {
    fn write_message(&mut self, message: UserMessage) {
        if message.text.contains("Waiting for another awman process") {
            self.waiting.notify_one();
        }
        self.messages.lock().unwrap().push(message.text);
    }
    fn replay_queued(&mut self) {}
}
#[tokio::test]
async fn concurrent_preflight_holds_build_leases_until_import_finishes() {
    let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::concurrent_preflight_holds_build_leases_until_import_finishes"));
    let store = Arc::new(Store::default());
    store.hold_import.store(true, Ordering::SeqCst);
    let tag = agent_image_tag(&s.work(), "claude");
    let first = preparer(&s, store.clone(), true, local());
    let second = preparer(&s, store.clone(), true, local());
    let first_tag = tag.clone();
    let one = tokio::spawn(async move {
        prepare_importing_step_with_recovery(
            first,
            &first_tag,
            "claude",
            &mut Sink::default(),
            None,
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        store.import_entered.notified(),
    )
    .await
    .unwrap();
    let waiting = Arc::new(tokio::sync::Notify::new());
    let mut sink = WaitingSink {
        messages: Arc::new(Mutex::new(vec![])),
        waiting: waiting.clone(),
    };
    let two = tokio::spawn(async move {
        prepare_importing_step_with_recovery(second, &tag, "claude", &mut sink, None).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), waiting.notified())
        .await
        .unwrap();
    assert_eq!(store.built.lock().unwrap().len(), 2);
    assert_eq!(store.imports.lock().unwrap().len(), 1);
    *store.release_import.0.lock().unwrap() = true;
    store.release_import.1.notify_all();
    one.await.unwrap().unwrap();
    two.await.unwrap().unwrap();
    assert_eq!(store.built.lock().unwrap().len(), 2);
    assert_eq!(store.imports.lock().unwrap().len(), 2);
    s.finish();
}

// ─── WI 0163: session setup and workflow preflight rebuild stale images ────

mod stale_images {
    use super::*;
    use crate::command::commands::agent_setup::NullAgentFrontend;
    use crate::data::config::env::EnvSnapshot;
    use crate::data::execution_event::{ImageStaleWarning, StaleImageKind};
    use crate::data::fs::{ApiPaths, AuthPathResolver};
    use crate::data::image_inputs::{agent_inputs_hash, project_inputs, DirBuildContext};
    use crate::data::session::{AgentName, SessionOpenOptions};
    use crate::data::setup_step::SetupStep;
    use crate::data::step_status::StepStatus;
    use crate::engine::agent::image_freshness::test_runtime::BuildRuntime;
    use crate::engine::agent::AgentEngine;
    use crate::engine::agent_runtime::AgentRuntimeEngine;
    use crate::engine::{auth::AuthEngine, git::GitEngine, overlay::OverlayEngine};

    /// What a frontend saw: messages and structured `image_stale` reports.
    #[derive(Default, Clone)]
    struct Seen {
        messages: Arc<Mutex<Vec<String>>>,
        stale: Arc<Mutex<Vec<ImageStaleWarning>>>,
        waiting: Option<Arc<tokio::sync::Notify>>,
    }
    impl UserMessageSink for Seen {
        fn write_message(&mut self, message: UserMessage) {
            if message.text.contains("Waiting for another awman process") {
                if let Some(waiting) = &self.waiting {
                    waiting.notify_one();
                }
            }
            self.messages.lock().unwrap().push(message.text);
        }
        fn replay_queued(&mut self) {}
        fn report_image_stale(&mut self, warning: &ImageStaleWarning) {
            self.stale.lock().unwrap().push(warning.clone());
        }
    }
    impl crate::engine::agent::AgentImageFrontend for Seen {
        fn report_step_status(&mut self, step: &SetupStep, status: StepStatus) {
            self.messages
                .lock()
                .unwrap()
                .push(format!("{step}: {}", status.label()));
        }
        fn container_frontend(
            &mut self,
        ) -> Box<dyn crate::engine::agent_runtime::frontend::AgentFrontend> {
            Box::new(NullAgentFrontend)
        }
    }

    struct Repo {
        session: Session,
        runtime: Arc<BuildRuntime>,
        agents: Arc<AgentEngine>,
        env: EnvSnapshot,
    }

    fn repo(s: &TestSandbox) -> Repo {
        let root = s.work();
        std::fs::create_dir_all(root.join(".awman")).unwrap();
        std::fs::write(
            root.join("Dockerfile.dev"),
            "FROM scratch\nCOPY tool.sh /tool.sh\n",
        )
        .unwrap();
        std::fs::write(root.join("tool.sh"), "echo v1\n").unwrap();
        std::fs::write(root.join(".awman/Dockerfile.claude"), "FROM project\n").unwrap();
        let env = EnvSnapshot::from_full_env(
            s.env()
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        );
        let session = Session::open_at_directory_root(
            root,
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let runtime = Arc::new(BuildRuntime::default());
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(
            AuthPathResolver::at_home(s.home()),
        ));
        let agents = Arc::new(AgentEngine::new(overlay, runtime.clone()));
        Repo {
            session,
            runtime,
            agents,
            env,
        }
    }

    impl Repo {
        fn project_tag(&self) -> String {
            project_image_tag(self.session.git_root())
        }
        fn agent_tag(&self) -> String {
            agent_image_tag(self.session.git_root(), "claude")
        }
        /// The working tree's project inputs hash.
        fn project_hash(&self) -> String {
            let root = self.session.git_root();
            let bytes = std::fs::read(root.join("Dockerfile.dev")).unwrap();
            project_inputs(Some(&bytes), &DirBuildContext::new(root))
                .hash
                .as_str()
                .to_string()
        }
        /// The agent inputs hash over whatever project image is installed.
        fn agent_hash(&self) -> String {
            let bytes =
                std::fs::read(self.session.git_root().join(".awman/Dockerfile.claude")).unwrap();
            let project = self.runtime.id(&self.project_tag()).unwrap();
            agent_inputs_hash(&bytes, &project).as_str().to_string()
        }
        /// Install images that match the working tree, as `ready` leaves them.
        fn install_fresh(&self) {
            self.runtime
                .install(&self.project_tag(), Some(&self.project_hash()));
            self.runtime
                .install(&self.agent_tag(), Some(&self.agent_hash()));
        }
        /// One session setup, as `chat` and `exec prompt` run it.
        async fn setup(&self, seen: &mut Seen) -> Result<(), EngineError> {
            self.agents
                .ensure_available_for_session(
                    &self.session,
                    &AgentName::new("claude").unwrap(),
                    seen,
                )
                .await
        }
        fn engines(&self, s: &TestSandbox) -> Engines {
            let auth = AuthPathResolver::at_home(s.home());
            let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
            Engines {
                runtime: self.runtime.clone(),
                container_runtime: None,
                sandbox_runtime: None,
                git_engine: Arc::new(GitEngine::with_env(self.env.clone())),
                overlay_engine: overlay,
                auth_engine: Arc::new(AuthEngine::with_paths(
                    auth,
                    ApiPaths::from_env(&self.env).unwrap(),
                )),
                agent_engine: self.agents.clone(),
                workflow_state_store: Arc::new(crate::data::WorkflowStateStore::new(&self.session)),
                credential_monitor: None,
                global_config: Arc::new(Default::default()),
                ephemeral_secrets: Arc::new(Default::default()),
            }
        }
    }

    #[tokio::test]
    async fn interleaved_project_retag_cannot_change_the_hashed_agent_parent() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::stale_images::interleaved_project_retag_cannot_change_the_hashed_agent_parent"));
        let repo = repo(&s);
        repo.install_fresh();
        let parent = repo.runtime.id(&repo.project_tag()).unwrap();
        let recipe = s.work().join(".awman/Dockerfile.claude");
        std::fs::write(
            &recipe,
            format!("FROM {}\nRUN echo changed\n", repo.project_tag()),
        )
        .unwrap();
        let bytes = std::fs::read(&recipe).unwrap();
        let expected = agent_inputs_hash(&bytes, &parent);
        repo.runtime
            .hold
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let retag = async {
            repo.runtime.build_entered.notified().await;
            repo.runtime
                .install(&repo.project_tag(), Some("other-commit"));
            repo.runtime.release();
        };
        let mut seen = Seen::default();
        let (result, ()) = tokio::join!(repo.setup(&mut seen), retag);
        result.unwrap();
        let recipes = repo.runtime.recipes.lock().unwrap();
        assert_eq!(recipes.len(), 1);
        assert!(
            recipes[0].contains(&format!("FROM {parent}\n")),
            "{}",
            recipes[0]
        );
        assert!(!recipes[0].contains(&format!("FROM {}", repo.project_tag())));
        assert_eq!(
            repo.runtime.label(&repo.agent_tag()).as_deref(),
            Some(expected.as_str())
        );
    }

    #[tokio::test]
    async fn stale_project_label_rebuilds_project_then_agent_once_and_the_next_session_reuses() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::stale_images::stale_project_label_rebuilds_project_then_agent_once_and_the_next_session_reuses"));
        let repo = repo(&s);
        repo.install_fresh();
        let mut seen = Seen::default();
        repo.setup(&mut seen).await.unwrap();
        assert!(repo.runtime.built().is_empty(), "fresh images are reused");

        // A file the project Dockerfile COPYs changes: the project label is
        // stale, and the agent image keys on the rebuilt project's id.
        std::fs::write(s.work().join("tool.sh"), "echo v2\n").unwrap();
        let old_agent = repo.runtime.id(&repo.agent_tag()).unwrap();
        repo.setup(&mut seen).await.unwrap();
        assert_eq!(repo.runtime.built(), [repo.project_tag(), repo.agent_tag()]);
        assert_eq!(
            repo.runtime.label(&repo.project_tag()),
            Some(repo.project_hash())
        );
        assert_eq!(
            repo.runtime.label(&repo.agent_tag()),
            Some(repo.agent_hash())
        );
        assert_ne!(repo.runtime.id(&repo.agent_tag()).unwrap(), old_agent);
        let messages = seen.messages.lock().unwrap().clone();
        assert!(messages
            .iter()
            .any(|m| m.contains("out of date with its Dockerfile")));
        assert!(messages.iter().any(|m| m.contains("builder stdout")));
        assert!(seen.stale.lock().unwrap().is_empty());

        // The next session sees matching labels and builds nothing.
        repo.setup(&mut Seen::default()).await.unwrap();
        assert_eq!(repo.runtime.built().len(), 2);

        // An agent Dockerfile change rebuilds only the agent image.
        std::fs::write(
            s.work().join(".awman/Dockerfile.claude"),
            "FROM project\nRUN true\n",
        )
        .unwrap();
        repo.setup(&mut Seen::default()).await.unwrap();
        assert_eq!(repo.runtime.built()[2..], [repo.agent_tag()]);
        s.finish();
    }

    #[tokio::test]
    async fn unlabeled_images_rebuild_once_then_converge_and_missing_project_still_fails() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::stale_images::unlabeled_images_rebuild_once_then_converge_and_missing_project_still_fails"));
        let repo = repo(&s);
        // No project image: setup fails exactly as before labels existed.
        let error = repo.setup(&mut Seen::default()).await.unwrap_err();
        assert!(matches!(
            error,
            EngineError::AgentRequiresProjectImage { .. }
        ));
        assert!(repo.runtime.built().is_empty());

        // Images from an older awman carry no inputs label.
        repo.runtime.install(&repo.project_tag(), None);
        repo.runtime.install(&repo.agent_tag(), None);
        let mut seen = Seen::default();
        repo.setup(&mut seen).await.unwrap();
        assert_eq!(repo.runtime.built(), [repo.project_tag(), repo.agent_tag()]);
        assert!(seen
            .messages
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.contains("has no inputs label")));
        for _ in 0..2 {
            repo.setup(&mut Seen::default()).await.unwrap();
        }
        assert_eq!(repo.runtime.built().len(), 2, "converged after one rebuild");
        s.finish();
    }

    #[tokio::test]
    async fn failed_rebuild_keeps_the_previous_image_reports_image_stale_and_a_repair_converges() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::stale_images::failed_rebuild_keeps_the_previous_image_reports_image_stale_and_a_repair_converges"));
        let repo = repo(&s);
        repo.install_fresh();
        let old_project = repo.runtime.id(&repo.project_tag()).unwrap();
        let old_label = repo.runtime.label(&repo.project_tag());
        // A broken Dockerfile.dev: the session still sets up on the old image.
        std::fs::write(s.work().join("Dockerfile.dev"), "FROM scratch\nRUN false\n").unwrap();
        repo.runtime.fail(&repo.project_tag(), true);
        let mut seen = Seen::default();
        repo.setup(&mut seen).await.unwrap();
        assert_eq!(repo.runtime.id(&repo.project_tag()), Some(old_project));
        let stale = seen.stale.lock().unwrap().clone();
        assert_eq!(stale.len(), 1);
        let warning = &stale[0];
        assert_eq!(warning.kind, StaleImageKind::Project);
        assert_eq!(warning.image, repo.project_tag());
        assert!(warning.build.contains(&repo.project_tag()));
        assert_eq!(warning.agent, None);
        assert_eq!(warning.expected_inputs, repo.project_hash());
        assert_eq!(warning.installed_inputs, old_label);
        assert_eq!(
            warning.reason,
            "exit code 1: ERROR: process \"false\" exited 1"
        );
        // The agent image still matches the kept project image: no build.
        assert_eq!(repo.runtime.built(), [repo.project_tag()]);

        // A failing agent rebuild is reported by agent, and also kept.
        repo.runtime.fail(&repo.project_tag(), false);
        repo.runtime.fail(&repo.agent_tag(), true);
        let mut seen = Seen::default();
        repo.setup(&mut seen).await.unwrap();
        let stale = seen.stale.lock().unwrap().clone();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].kind, StaleImageKind::Agent);
        assert_eq!(stale[0].agent.as_deref(), Some("claude"));
        assert_eq!(stale[0].image, repo.agent_tag());
        assert!(repo.runtime.image_exists(&repo.agent_tag()).unwrap());

        // Once the build works again the session converges.
        repo.runtime.fail(&repo.agent_tag(), false);
        let mut seen = Seen::default();
        repo.setup(&mut seen).await.unwrap();
        assert!(seen.stale.lock().unwrap().is_empty());
        assert_eq!(
            repo.runtime.label(&repo.agent_tag()),
            Some(repo.agent_hash())
        );
        let builds = repo.runtime.built().len();
        repo.setup(&mut Seen::default()).await.unwrap();
        assert_eq!(repo.runtime.built().len(), builds);
        s.finish();
    }

    #[tokio::test]
    async fn concurrent_preflights_with_a_changed_dockerfile_build_once() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::stale_images::concurrent_preflights_with_a_changed_dockerfile_build_once"));
        let repo = repo(&s);
        repo.install_fresh();
        std::fs::write(
            s.work().join("Dockerfile.dev"),
            "FROM scratch\nRUN echo v2\n",
        )
        .unwrap();
        repo.runtime.hold.store(true, Ordering::SeqCst);
        let engines = repo.engines(&s);
        let agents = vec!["claude".to_string()];

        let (first_engines, first_session, first_agents) =
            (engines.clone(), repo.session.clone(), agents.clone());
        let mut first_seen = Seen::default();
        let first_sink = first_seen.clone();
        let one = tokio::spawn(async move {
            refresh_stale_workflow_images(
                &first_engines,
                &first_session,
                &first_agents,
                &mut first_seen,
                None,
            )
            .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            repo.runtime.build_entered.notified(),
        )
        .await
        .unwrap();

        let waiting = Arc::new(tokio::sync::Notify::new());
        let mut second_seen = Seen {
            waiting: Some(waiting.clone()),
            ..Default::default()
        };
        let second_sink = second_seen.clone();
        let (second_engines, second_session) = (engines.clone(), repo.session.clone());
        let two = tokio::spawn(async move {
            refresh_stale_workflow_images(
                &second_engines,
                &second_session,
                &agents,
                &mut second_seen,
                None,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting.notified())
            .await
            .unwrap();
        assert_eq!(repo.runtime.built(), [repo.project_tag()]);
        repo.runtime.release();
        let first = one.await.unwrap().unwrap();
        let second = two.await.unwrap().unwrap();

        // One build per image; the second preflight found the new labels.
        assert_eq!(repo.runtime.built(), [repo.project_tag(), repo.agent_tag()]);
        // Whichever preflight reached each lease first built that image.
        let mut rebuilt = [first.rebuilt.clone(), second.rebuilt.clone()].concat();
        rebuilt.sort();
        let mut expected = vec![repo.project_tag(), repo.agent_tag()];
        expected.sort();
        assert_eq!(rebuilt, expected);
        assert_eq!(first.rebuilt.first(), Some(&repo.project_tag()));
        assert!(first.warnings.is_empty() && second.warnings.is_empty());
        assert_eq!(
            repo.runtime.label(&repo.project_tag()),
            Some(repo.project_hash())
        );
        assert_eq!(
            repo.runtime.label(&repo.agent_tag()),
            Some(repo.agent_hash())
        );
        assert!(first_sink.stale.lock().unwrap().is_empty());
        assert!(second_sink.stale.lock().unwrap().is_empty());
        s.finish();
    }

    #[tokio::test]
    async fn workflow_preflight_reports_a_failed_rebuild_through_the_build_output_target() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_preflight::image_contracts::stale_images::workflow_preflight_reports_a_failed_rebuild_through_the_build_output_target"));
        let repo = repo(&s);
        repo.install_fresh();
        std::fs::write(
            s.work().join(".awman/Dockerfile.claude"),
            "FROM project\nRUN false\n",
        )
        .unwrap();
        repo.runtime.fail(&repo.agent_tag(), true);
        let engines = repo.engines(&s);
        let mut seen = Seen::default();
        let mut output = Output::default();
        let refresh = refresh_stale_workflow_images(
            &engines,
            &repo.session,
            &["claude".into(), "claude".into()],
            &mut seen,
            Some(&mut output),
        )
        .await
        .unwrap();
        // Routed like any preflight build: raw lines to the target, the
        // warning to the frontend; duplicate agents build once.
        assert_eq!(output.begins, [repo.agent_tag()]);
        assert_eq!(output.lines, ["builder stdout"]);
        assert!(output.finishes[0].1.is_some());
        assert_eq!(refresh.warnings.len(), 1);
        assert_eq!(*seen.stale.lock().unwrap(), refresh.warnings);
        assert!(!seen
            .messages
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "builder stdout"));
        // The previous agent image is still there for the run.
        assert!(repo.runtime.image_exists(&repo.agent_tag()).unwrap());
        s.finish();
    }
}
