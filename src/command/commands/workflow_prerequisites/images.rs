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
            yolo_authorized: false,
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
        let plan = WorkflowPreflightPlan::for_workflow(&workflow, session, &flags)?;
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
            _: &WorkflowPrerequisiteReport,
            next: PrerequisiteReportNext,
        ) -> Result<(), CommandError> {
            self.reports += 1;
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
}
