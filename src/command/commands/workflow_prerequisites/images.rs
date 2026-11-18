//! Image preparation contracts through the combined workflow prerequisite flow.
#[cfg(test)]
mod contracts {
    use super::super::*;
    use crate::command::{
        dispatch::{catalogue::FrontendKind, Engines},
        error::CommandError,
    };
    use crate::data::{
        session::Session,
        workflow_definition::{Workflow, WorkflowFormat},
        workflow_prerequisites::{
            PrerequisiteOrigin, PrerequisiteStatus, WorkflowPrerequisiteReport,
        },
    };

    async fn prepare_images(
        engines: &Engines,
        session: &Session,
        agents: &[String],
        origin: PrerequisiteOrigin,
        check_only: bool,
        frontend: &mut dyn WorkflowPrerequisiteFrontend,
    ) -> Result<WorkflowPrerequisiteReport, CommandError> {
        let mut source = String::from("title = 'images'\n");
        for (index, agent) in agents.iter().enumerate() {
            source.push_str(&format!(
                "[[steps]]\nname = 'step-{index}'\nagent = '{agent}'\nprompt = 'literal'\n"
            ));
        }
        let workflow = Workflow::parse(&source, WorkflowFormat::Toml)?;
        let flags = crate::command::commands::exec_workflow::ExecWorkflowCommandFlags {
            workflow: None,
            work_item: None,
            non_interactive: false,
            plan: false,
            allow_docker: false,
            worktree: false,
            yolo: false,
            auto: false,
            agent: None,
            model: None,
            launch_mode: None,
            overlay: vec![],
            agent_flags: None,
            max_concurrent: None,
            dynamic: false,
            leader: None,
            on_dirty_merge_target: None,
            on_merge_failure: None,
            on_existing_worktree: None,
            prerequisites: Default::default(),
            squad_task_prerequisites: None,
        };
        let plan = if origin == PrerequisiteOrigin::AssumedAll {
            WorkflowPreflightPlan::dynamic(agents.to_vec(), session, &flags)?
        } else {
            WorkflowPreflightPlan::for_workflow(&workflow, session, &flags)?
        };
        // These contracts exercise image lifecycle independently of credentials.
        let mut plan = plan;
        plan.needs.needs.retain(|need| {
            matches!(
                need.prerequisite,
                crate::data::workflow_prerequisites::WorkflowPrerequisite::AgentImage { .. }
            )
        });
        let options = WorkflowPrerequisiteFlowOptions::for_session(
            session,
            FrontendKind::Cli,
            &Default::default(),
            None,
            origin,
            None,
        );
        let headless = (!frontend.can_ask_prerequisites()).then(|| options.headless.clone());
        let flow = WorkflowPrerequisiteFlow::new(engines, session, options);
        if check_only {
            return Ok(flow.preflight(&plan.needs, false));
        }
        let mut prepared = flow.prepare(&plan.needs, frontend)?;
        plan.prepare_images(&mut prepared.report, engines, session, headless, frontend)
            .await?;
        if origin == PrerequisiteOrigin::AssumedAll {
            prepared.narrow_to(&workflow, session, &flags, frontend)?;
        }
        frontend.show_workflow_prerequisites(&prepared.report, PrerequisiteReportNext::Decided)?;
        Ok(prepared.report)
    }
    use crate::data::{
        config::env::EnvSnapshot,
        fs::{ApiPaths, AuthPathResolver},
        message::{UserMessage, UserMessageSink},
        prompt::Prompt,
        session::SessionOpenOptions,
    };
    use crate::engine::{
        agent::AgentEngine, auth::AuthEngine, git::GitEngine, image_prep::contracts::Store,
        overlay::OverlayEngine,
    };
    use awman_test_support::{TestCase, TestSandbox};
    use std::sync::Arc;
    #[derive(Default)]
    struct Frontend {
        interactive: bool,
        accept: bool,
        asked: usize,
        reports: usize,
        observed: Vec<WorkflowPrerequisiteReport>,
    }
    impl UserMessageSink for Frontend {
        fn write_message(&mut self, _: UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl WorkflowPrerequisiteFrontend for Frontend {
        fn can_ask_prerequisites(&self) -> bool {
            self.interactive
        }
        fn ask_prerequisite_image(&mut self, _: &Prompt<bool>) -> Result<bool, CommandError> {
            panic!("image preparation belongs to the combined prerequisite review")
        }
        fn show_workflow_prerequisites(
            &mut self,
            report: &WorkflowPrerequisiteReport,
            next: PrerequisiteReportNext,
        ) -> Result<(), CommandError> {
            self.reports += 1;
            self.observed.push(report.clone());
            if matches!(next, PrerequisiteReportNext::Questions { .. }) {
                assert!(self.interactive, "headless must not ask");
                self.asked += 1;
                if !self.accept {
                    return Err(CommandError::Aborted);
                }
            }
            Ok(())
        }
    }

    fn build_fixture(
        s: &TestSandbox,
        agents: &[&str],
    ) -> (
        Session,
        Engines,
        Arc<crate::engine::agent::image_freshness::test_runtime::BuildRuntime>,
    ) {
        use crate::data::image_inputs::agent_inputs_hash;
        let root = s.work();
        std::fs::create_dir_all(root.join(".awman")).unwrap();
        // Exercise the configured Dockerfile and COPY inputs, not just its text.
        std::fs::write(
            root.join(".awman/config.json"),
            r#"{"runtime":{"dockerfile":"Projectfile"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("Projectfile"), "FROM scratch\nCOPY tool /tool\n").unwrap();
        std::fs::write(root.join("tool"), "v1").unwrap();
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
            root.clone(),
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let runtime =
            Arc::new(crate::engine::agent::image_freshness::test_runtime::BuildRuntime::default());
        let hash =
            crate::engine::image_prep::project_inputs_from_disk(&root, &root.join("Projectfile"))
                .unwrap()
                .unwrap();
        let parent = runtime.install(&session.project_image_tag(), Some(hash.as_str()));
        for agent in agents {
            let recipe = format!("FROM {}\n", session.project_image_tag());
            std::fs::write(root.join(format!(".awman/Dockerfile.{agent}")), &recipe).unwrap();
            runtime.install(
                &session.agent_image_tag(agent),
                Some(agent_inputs_hash(recipe.as_bytes(), &parent).as_str()),
            );
        }
        let auth = AuthPathResolver::at_home(s.home());
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
        let engines = Engines {
            runtime: runtime.clone(),
            container_runtime: None,
            sandbox_runtime: None,
            git_engine: Arc::new(GitEngine::with_env(env.clone())),
            overlay_engine: overlay.clone(),
            auth_engine: Arc::new(AuthEngine::with_paths(
                auth,
                ApiPaths::from_env(&env).unwrap(),
            )),
            agent_engine: Arc::new(AgentEngine::new(overlay, runtime.clone())),
            workflow_state_store: Arc::new(crate::data::WorkflowStateStore::new(&session)),
            credential_monitor: None,
            global_config: Arc::new(Default::default()),
            ephemeral_secrets: Arc::new(Default::default()),
            lease_workflow_review: None,
        };
        (session, engines, runtime)
    }

    #[tokio::test]
    async fn stale_images_are_reviewed_before_building_and_fresh_images_are_reused() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_prerequisites::images::contracts::stale_images_are_reviewed_before_building_and_fresh_images_are_reused"));
        let (session, engines, runtime) = build_fixture(&s, &["claude", "codex"]);
        let agents = vec!["claude".into(), "codex".into()];
        let mut frontend = Frontend {
            interactive: true,
            accept: true,
            ..Default::default()
        };
        let check = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            true,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(check.all_satisfied(), "{check:?}");
        assert!(runtime.built().is_empty());
        let path = s.work().join(".awman/Dockerfile.codex");
        let changed = format!("FROM {}\nRUN echo changed\n", session.project_image_tag());
        std::fs::write(&path, &changed).unwrap();
        for origin in [PrerequisiteOrigin::Static, PrerequisiteOrigin::AssumedAll] {
            let check = prepare_images(&engines, &session, &agents, origin, true, &mut frontend)
                .await
                .unwrap();
            assert_eq!(WorkflowPrerequisiteFlow::check_only_exit_code(&check), 1);
            assert_eq!(check.checks[0].status, PrerequisiteStatus::Satisfied);
            assert_eq!(check.checks[1].status, PrerequisiteStatus::Missing);
            assert!(check.checks[1].explanation.contains("rebuild required"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
        }
        assert!(runtime.built().is_empty(), "check-only never builds");
        frontend.accept = false;
        assert!(matches!(
            prepare_images(
                &engines,
                &session,
                &agents,
                PrerequisiteOrigin::Static,
                false,
                &mut frontend
            )
            .await,
            Err(CommandError::Aborted)
        ));
        assert!(runtime.built().is_empty(), "aborting review never builds");
        frontend.accept = true;
        let result = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            false,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(result.all_satisfied(), "{result:?}");
        assert_eq!(runtime.built(), [session.agent_image_tag("codex")]);
        assert_eq!(
            frontend.observed[0].checks[1].status,
            PrerequisiteStatus::Missing
        );
        let result = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            false,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(result.all_satisfied());
        assert_eq!(runtime.built().len(), 1);
        std::fs::remove_file(&path).unwrap();
        let check = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            true,
            &mut frontend,
        )
        .await
        .unwrap();
        assert_eq!(check.checks[1].status, PrerequisiteStatus::Missing);
        assert!(check.checks[1].explanation.contains("downloaded"));
        assert!(
            !path.exists(),
            "check-only must not download a missing recipe"
        );
        assert_eq!(runtime.built().len(), 1);
        runtime
            .images
            .lock()
            .unwrap()
            .remove(&session.project_image_tag());
        let check = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            true,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(check
            .checks
            .iter()
            .all(|check| check.status == PrerequisiteStatus::Missing
                && check.explanation.contains("run awman ready")));
        s.finish();
    }

    #[tokio::test]
    async fn project_changes_invalidate_every_agent_and_failed_refresh_stays_visible() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_prerequisites::images::contracts::project_changes_invalidate_every_agent_and_failed_refresh_stays_visible"));
        let (session, engines, runtime) = build_fixture(&s, &["claude", "codex"]);
        let agents = vec!["claude".into(), "codex".into()];
        let mut frontend = Frontend::default(); // unattended refresh also works
        std::fs::write(s.work().join("tool"), "v2").unwrap();
        let report = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::AssumedAll,
            true,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(report
            .checks
            .iter()
            .all(|check| check.status == PrerequisiteStatus::Missing
                && check.explanation.contains(&session.project_image_tag())));
        assert!(runtime.built().is_empty());
        let report = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::AssumedAll,
            false,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(report.all_satisfied(), "{report:?}");
        assert_eq!(
            runtime.built(),
            [
                session.project_image_tag(),
                session.agent_image_tag("claude"),
                session.agent_image_tag("codex")
            ]
        );
        let tag = session.agent_image_tag("codex");
        runtime.install(&tag, None);
        runtime.fail(&tag, true);
        let old = runtime.id(&tag);
        let check = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            true,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(check.checks[1].explanation.contains("no inputs label"));
        let report = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::AssumedAll,
            false,
            &mut frontend,
        )
        .await
        .unwrap();
        assert_eq!(
            runtime.id(&tag),
            old,
            "failed refresh preserves installed image"
        );
        assert_eq!(
            runtime.built().len(),
            4,
            "one refresh attempt, no missing-image retry"
        );
        assert_eq!(report.checks[1].status, PrerequisiteStatus::Missing);
        assert!(report.checks[1]
            .explanation
            .contains("continuing with the installed image"));
        runtime.fail(&tag, false);
        let report = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            false,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(report.all_satisfied());
        assert_eq!(runtime.built().len(), 5);
        s.finish();
    }
    #[tokio::test]
    async fn recipe_updates_are_previewed_without_writes_and_inspection_errors_stay_unknown() {
        use crate::data::{image_inputs::agent_inputs_hash, session::AgentName};
        use crate::engine::agent::{provision_agent_dockerfile, AgentRegistry};
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_prerequisites::images::contracts::recipe_updates_are_previewed_without_writes_and_inspection_errors_stay_unknown"));
        let (session, engines, runtime) = build_fixture(&s, &["worker"]);
        let root = s.work();
        let definitions = root.join(".awman/agents");
        std::fs::create_dir_all(&definitions).unwrap();
        let name = AgentName::new("worker").unwrap();
        let approve = |version: &str| {
            std::fs::write(definitions.join("worker.toml"), format!("schema = 1\ndisplay_name = 'Worker'\n[run]\ncommand = ['worker']\n[image]\nneeds_node = false\ninstall = ['echo {version}']\n")).unwrap();
            let def = AgentRegistry::load_untrusted(session.env(), &name, Some(&root)).unwrap();
            AgentRegistry::record_approval(session.env(), def.definition(), Some(&root)).unwrap();
            AgentRegistry::resolve_for_session(&session, &name).unwrap()
        };
        let def = approve("v1");
        let paths = crate::data::RepoDockerfilePaths::new(&root);
        let recipe_path = paths.agent_dockerfile("worker");
        std::fs::remove_file(&recipe_path).unwrap();
        provision_agent_dockerfile(&def, &paths, &session.project_image_tag())
            .await
            .unwrap();
        let old_recipe = std::fs::read(&recipe_path).unwrap();
        let parent = runtime.id(&session.project_image_tag()).unwrap();
        runtime.install(
            &session.agent_image_tag("worker"),
            Some(agent_inputs_hash(&old_recipe, &parent).as_str()),
        );
        approve("v2");
        let agents = vec!["worker".into()];
        let mut frontend = Frontend {
            interactive: true,
            accept: true,
            ..Default::default()
        };
        let report = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            true,
            &mut frontend,
        )
        .await
        .unwrap();
        assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
        assert_eq!(
            std::fs::read(&recipe_path).unwrap(),
            old_recipe,
            "check-only must preview the new recipe without publishing it"
        );
        assert!(runtime.built().is_empty());
        let report = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            false,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(report.all_satisfied(), "{report:?}");
        assert_ne!(std::fs::read(&recipe_path).unwrap(), old_recipe);
        assert_eq!(runtime.built(), [session.agent_image_tag("worker")]);
        // An owned Dockerfile is the effective recipe even if its definition changes.
        let owned = format!("FROM {}\nRUN echo owned\n", session.project_image_tag());
        std::fs::write(&recipe_path, &owned).unwrap();
        runtime.install(
            &session.agent_image_tag("worker"),
            Some(agent_inputs_hash(owned.as_bytes(), &parent).as_str()),
        );
        approve("v3");
        let report = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            true,
            &mut frontend,
        )
        .await
        .unwrap();
        assert!(report.all_satisfied(), "{report:?}");
        assert_eq!(std::fs::read_to_string(&recipe_path).unwrap(), owned);
        *runtime.inspection_error.lock().unwrap() = Some("cannot inspect metadata".into());
        let report = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            true,
            &mut frontend,
        )
        .await
        .unwrap();
        assert_eq!(report.checks[0].status, PrerequisiteStatus::Unknown);
        assert!(report.checks[0]
            .explanation
            .contains("cannot inspect metadata"));
        assert_eq!(runtime.built().len(), 1);
        s.finish();
    }

    #[tokio::test]
    async fn image_prerequisites_check_only_cancel_and_headless_defaults() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_prerequisites::images::contracts::image_prerequisites_check_only_cancel_and_headless_defaults"));
        std::fs::create_dir_all(s.work().join(".awman")).unwrap();
        std::fs::write(s.work().join(".awman/config.json"), r#"{"runtime":{"builtin":{"builder":{"enabled":true},"imageSource":{"type":"registry"}}}}"#).unwrap();
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
            s.work(),
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let runtime = Arc::new(Store::default());
        let auth = AuthPathResolver::at_home(s.home());
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
        let engines = Engines {
            runtime: runtime.clone(),
            container_runtime: None,
            sandbox_runtime: None,
            git_engine: Arc::new(GitEngine::with_env(env.clone())),
            overlay_engine: overlay.clone(),
            auth_engine: Arc::new(AuthEngine::with_paths(
                auth,
                ApiPaths::from_env(&env).unwrap(),
            )),
            agent_engine: Arc::new(AgentEngine::new(overlay, runtime.clone())),
            workflow_state_store: Arc::new(crate::data::WorkflowStateStore::new(&session)),
            credential_monitor: None,
            global_config: Arc::new(Default::default()),
            ephemeral_secrets: Arc::new(Default::default()),
            lease_workflow_review: None,
        };
        let agents = vec!["claude".into(), "claude".into()];
        let mut frontend = Frontend {
            interactive: true,
            ..Default::default()
        };
        let report = prepare_images(
            &engines,
            &session,
            &agents,
            PrerequisiteOrigin::Static,
            true,
            &mut frontend,
        )
        .await
        .unwrap();
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, PrerequisiteStatus::Missing);
        let mut failed = report.clone();
        failed.checks[0].explanation =
            "image build exited with code 64\nbuilder stderr: invalid Dockerfile".into();
        let error = CommandError::WorkflowPrerequisitesUnresolved {
            report: Box::new(failed),
        };
        assert!(error
            .to_string()
            .contains("builder stderr: invalid Dockerfile"));
        assert_eq!(frontend.asked, 0);
        assert!(runtime.imports.lock().unwrap().is_empty());
        assert!(matches!(
            prepare_images(
                &engines,
                &session,
                &agents,
                PrerequisiteOrigin::Static,
                false,
                &mut frontend
            )
            .await,
            Err(CommandError::Aborted)
        ));
        assert!(runtime.imports.lock().unwrap().is_empty());
        assert_eq!(frontend.asked, 1);
        for interactive in [false, true] {
            runtime.installed.lock().unwrap().clear();
            let mut frontend = Frontend {
                interactive,
                accept: true,
                ..Default::default()
            };
            let report = prepare_images(
                &engines,
                &session,
                &agents,
                PrerequisiteOrigin::Static,
                false,
                &mut frontend,
            )
            .await
            .unwrap();
            assert_eq!(report.checks[0].status, PrerequisiteStatus::Satisfied);
            assert_eq!(frontend.asked, usize::from(interactive));
            assert!(frontend.reports > 0);
        }
        assert_eq!(runtime.imports.lock().unwrap().len(), 2);
        for policy in [
            crate::command::headless::HeadlessDefaults::cli(),
            crate::command::headless::HeadlessDefaults::api(),
        ] {
            assert!(policy.prerequisite_image());
        }
        s.finish();
    }

    /// Workflow preflight prepares images without `ReadyCommand`: a missing
    /// builtin source is reported, never prompted for, and no config file is
    /// written whichever way the review is answered.
    #[tokio::test]
    async fn workflow_image_prerequisites_leave_config_unchanged_without_a_source() {
        let s = TestSandbox::for_test(TestCase::component("command::commands::workflow_prerequisites::images::contracts::workflow_image_prerequisites_leave_config_unchanged_without_a_source"));
        std::fs::create_dir_all(s.work().join(".awman")).unwrap();
        std::fs::write(s.work().join("Dockerfile.dev"), "FROM scratch\n").unwrap();
        let repo = s.work().join(".awman/config.json");
        std::fs::write(&repo, r#"{"runtime":{"builtin":{"vcpus":2}}}"#).unwrap();
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
        let global = crate::data::config::global_config_path(&env).unwrap();
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(&global, r#"{"runtime":{"kind":"builtin"}}"#).unwrap();
        let before = (
            std::fs::read(&repo).unwrap(),
            std::fs::read(&global).unwrap(),
        );
        let session = Session::open_at_git_root(
            s.work(),
            s.work(),
            SessionOpenOptions {
                env: Some(env.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let runtime = Arc::new(Store::default());
        let auth = AuthPathResolver::at_home(s.home());
        let overlay = Arc::new(OverlayEngine::with_auth_resolver(auth.clone()));
        let engines = Engines {
            runtime: runtime.clone(),
            container_runtime: None,
            sandbox_runtime: None,
            git_engine: Arc::new(GitEngine::with_env(env.clone())),
            overlay_engine: overlay.clone(),
            auth_engine: Arc::new(AuthEngine::with_paths(
                auth,
                ApiPaths::from_env(&env).unwrap(),
            )),
            agent_engine: Arc::new(AgentEngine::new(overlay, runtime.clone())),
            workflow_state_store: Arc::new(crate::data::WorkflowStateStore::new(&session)),
            credential_monitor: None,
            global_config: Arc::new(Default::default()),
            ephemeral_secrets: Arc::new(Default::default()),
            lease_workflow_review: None,
        };
        let agents = vec!["claude".to_string()];
        for (interactive, check_only) in [(true, true), (true, false), (false, false)] {
            let mut frontend = Frontend {
                interactive,
                accept: true,
                ..Default::default()
            };
            let outcome = prepare_images(
                &engines,
                &session,
                &agents,
                PrerequisiteOrigin::Static,
                check_only,
                &mut frontend,
            )
            .await;
            let report = match outcome {
                Ok(report) => report,
                Err(CommandError::WorkflowPrerequisitesUnresolved { report }) => *report,
                Err(other) => panic!("unexpected error: {other}"),
            };
            assert_ne!(report.checks[0].status, PrerequisiteStatus::Satisfied);
            assert!(
                report.checks[0]
                    .explanation
                    .contains(&session.agent_image_tag("claude")),
                "{}",
                report.checks[0].explanation
            );
            // Review finding 6: the public report says the source is missing
            // and names the full keys that fix it.
            let tag = session.agent_image_tag("claude");
            for needle in [
                "no image source is configured".to_string(),
                "`runtime.builtin.imageSource`".to_string(),
                format!("`runtime.builtin.images[\"{tag}\"]`"),
            ] {
                assert!(
                    report.checks[0].explanation.contains(&needle),
                    "interactive={interactive} check_only={check_only}: {needle} missing from {}",
                    report.checks[0].explanation
                );
            }
            assert_eq!(
                (
                    std::fs::read(&repo).unwrap(),
                    std::fs::read(&global).unwrap()
                ),
                before
            );
        }
        assert!(runtime.imports.lock().unwrap().is_empty());
        s.finish();
    }
}
