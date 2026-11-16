//! Image preparation is a prerequisite solve, shared by interactive and
//! unattended workflows. Check-only reports never acquire or build images.
use super::{PrerequisiteReportNext, WorkflowPrerequisiteFrontend};
use crate::command::{dispatch::Engines, error::CommandError};
use crate::data::{
    config::workflows::UnresolvedMode,
    image_tags::agent_image_tag,
    session::Session,
    workflow_prerequisites::{
        PrerequisiteCheck, PrerequisiteOrigin, PrerequisitePhase, PrerequisiteStatus, Remediation,
        WorkflowPrerequisite, WorkflowPrerequisiteReport,
    },
};
use crate::engine::{
    agent_runtime::ImageAcquisition,
    image_prep::{AgentImagePreparer, BuildPolicy, ImageBuildContext},
};

pub(crate) async fn prepare_images(
    engines: &Engines,
    session: &Session,
    agents: &[String],
    origin: PrerequisiteOrigin,
    check_only: bool,
    frontend: &mut dyn WorkflowPrerequisiteFrontend,
) -> Result<WorkflowPrerequisiteReport, CommandError> {
    let mut report = WorkflowPrerequisiteReport {
        phase: PrerequisitePhase::Start,
        origin,
        checks: vec![],
        advisories: vec![],
        notes: vec![],
    };
    if agents.is_empty()
        || engines.runtime.capabilities().image_acquisition != ImageAcquisition::Import
    {
        return Ok(report);
    }
    let config = session.effective_config();
    let mut preparer = AgentImagePreparer::new(
        config.builtin_settings()?,
        engines.runtime.clone(),
        crate::data::fs::data_paths::data_home(config.env())?,
        ImageBuildContext::for_session(session, session.configured_agent().as_str()),
    )
    .with_definition_env(session.env().clone());
    for agent in agents {
        let tag = agent_image_tag(session.git_root(), agent);
        if report.checks.iter().any(|check| matches!(&check.prerequisite, WorkflowPrerequisite::AgentImage { tag: existing, .. } if existing == &tag)) { continue; }
        let installed = engines.runtime.image_exists(&tag)?;
        let policy = preparer.missing_image_policy(&tag, agent, BuildPolicy::ImportOnly)?;
        let explanation = if installed {
            format!("{tag} is installed; verify its configured source without rebuilding")
        } else if policy == BuildPolicy::SessionSetup
            && preparer.missing_image_plan(&tag, agent)
                == crate::data::ready_image::ReadyImagePlan::BuildAndImport
        {
            format!("{tag} is missing; build base and agent images only if absent from the configured local source store, then import")
        } else {
            format!("{tag} is missing; import from the configured source (no local build)")
        };
        report.checks.push(PrerequisiteCheck {
            prerequisite: WorkflowPrerequisite::AgentImage {
                agent: agent.clone(),
                tag,
            },
            status: if installed {
                PrerequisiteStatus::Satisfied
            } else {
                PrerequisiteStatus::Missing
            },
            explanation,
            needed_by: vec![],
            remediations: if installed {
                vec![]
            } else {
                vec![Remediation::PrepareImage, Remediation::Abort]
            },
        });
    }
    if check_only || report.checks.is_empty() {
        return Ok(report);
    }
    frontend.begin_workflow_prerequisites();
    let can_ask = frontend.can_ask_prerequisites();
    if can_ask && !report.needs_attention().is_empty() {
        frontend.show_workflow_prerequisites(
            &report,
            PrerequisiteReportNext::Questions {
                unresolved: UnresolvedMode::Fail,
            },
        )?;
    }
    // Ask every choice before starting builds, so cancelling leaves images
    // untouched. All headless profiles prepare without asking.
    for check in report.needs_attention() {
        let prompt = crate::command::prompts::prerequisite_image(check);
        let prepare = if can_ask {
            frontend.ask_prerequisite_image(&prompt)?
        } else {
            crate::command::headless::HeadlessDefaults::cli().prerequisite_image()
        };
        if !prepare {
            return Err(CommandError::Aborted);
        }
    }
    let paths = crate::data::RepoDockerfilePaths::new(session.git_root());
    for index in 0..report.checks.len() {
        let WorkflowPrerequisite::AgentImage { agent, .. } = &report.checks[index].prerequisite
        else {
            unreachable!()
        };
        let result = crate::command::commands::workflow_preflight::ensure_agent_image(
            engines,
            session.git_root(),
            &paths,
            agent,
            session,
            frontend,
        )
        .await;
        let check = &mut report.checks[index];
        if let Err(error) = result {
            check.status = PrerequisiteStatus::Missing;
            check.explanation = error.to_string();
            frontend.show_workflow_prerequisites(&report, PrerequisiteReportNext::Decided)?;
            return Err(CommandError::WorkflowPrerequisitesUnresolved {
                report: Box::new(report),
            });
        }
        check.status = PrerequisiteStatus::Satisfied;
        check.explanation = "image prepared for workflow execution".into();
        check.remediations.clear();
    }
    frontend.show_workflow_prerequisites(&report, PrerequisiteReportNext::Decided)?;
    Ok(report)
}

#[cfg(test)]
mod contracts {
    use super::*;
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
            assert!(self.interactive, "headless must not ask");
            self.asked += 1;
            Ok(self.accept)
        }
        fn show_workflow_prerequisites(
            &mut self,
            _: &WorkflowPrerequisiteReport,
            _: PrerequisiteReportNext,
        ) -> Result<(), CommandError> {
            self.reports += 1;
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
