//! Backend policy against a controlled transport: no hypervisor or provider.
use super::{backend::BuiltinBackend, driver::*, naming, paths::BuiltinPaths};
use crate::{
    data::session::AgentHandle,
    engine::{
        agent_runtime::AgentStats,
        container::{backend::ContainerBackend, runtime::BuiltinRuntimeSettings},
        error::EngineError,
    },
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Driver(Mutex<Option<SandboxSummary>>);
impl SandboxDriver for Driver {
    fn hypervisor_available(&self) -> Result<(), EngineError> {
        Ok(())
    }
    fn create(&self, spec: SandboxSpec) -> Result<(), EngineError> {
        *self.0.lock().unwrap() = Some(SandboxSummary {
            handle: AgentHandle {
                id: spec.name,
                name: spec.labels[naming::LABEL_NAME].clone(),
                image_tag: spec.image,
                started_at: chrono::Utc::now(),
            },
            labels: spec.labels,
            stopped: false,
        });
        Ok(())
    }
    fn get(&self, id: &str) -> Result<SandboxSummary, EngineError> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| EngineError::AgentNotFound { id: id.into() })
    }
    fn list(&self) -> Result<Vec<SandboxSummary>, EngineError> {
        Ok(self.0.lock().unwrap().clone().into_iter().collect())
    }
    fn exec(&self, _: &str, _: ExecRequest) -> Result<ExecSession, EngineError> {
        unreachable!()
    }
    fn finish_owned(
        &self,
        _: &str,
        owner: &str,
        _: Duration,
        remove: bool,
    ) -> Result<(), EngineError> {
        let mut state = self.0.lock().unwrap();
        if let Some(current) = state.as_mut() {
            naming::check_owner(&current.labels, owner)?;
            current.stopped = true;
            if remove {
                *state = None;
            }
        }
        Ok(())
    }
    fn remove_stopped_checked(
        &self,
        handle: &AgentHandle,
        owner: &str,
        execution: &str,
    ) -> Result<(), EngineError> {
        let mut state = self.0.lock().unwrap();
        if let Some(current) = state.as_ref() {
            naming::check_owner(&current.labels, owner)?;
            if !current.stopped
                || current.handle.started_at != handle.started_at
                || current.handle.name != handle.name
                || current
                    .labels
                    .get("awman.profile-execution")
                    .map(String::as_str)
                    != Some(execution)
            {
                return Err(EngineError::Other(
                    "sandbox generation or execution changed".into(),
                ));
            }
        }
        *state = None;
        Ok(())
    }
    fn remove_stopped(&self, _: &str, owner: &str) -> Result<(), EngineError> {
        let mut state = self.0.lock().unwrap();
        if let Some(current) = state.as_ref() {
            naming::check_owner(&current.labels, owner)?;
            if !current.stopped {
                return Err(EngineError::Other("still running".into()));
            }
        }
        *state = None;
        Ok(())
    }
    fn stats(&self, _: &str) -> Result<AgentStats, EngineError> {
        Err(EngineError::Other("no metrics".into()))
    }
    fn image_config(&self, _: &str) -> Result<Option<SandboxImageConfig>, EngineError> {
        Ok(Some(SandboxImageConfig::default()))
    }
    fn import_archive(&self, _: PathBuf, _: String) -> Result<(), EngineError> {
        unreachable!()
    }
    fn remove_image(&self, _: &str) -> Result<(), EngineError> {
        unreachable!()
    }
}

#[test]
fn phase_launch_contract_carries_grace_and_spooled_transcript() {
    use crate::engine::container::{
        options::ResolvedContainerOptions, phase_step::PhaseStepContainerSpec,
    };
    let s = awman_test_support::TestSandbox::new();
    let driver = Arc::new(Driver::default());
    let backend = backend(&s, driver);
    let settings = crate::data::config::builtin_launch::BuiltinLaunchSettings::default();
    let env = Default::default();
    let spec = PhaseStepContainerSpec {
        builtin_launch: Some(&settings),
        image: "fixture",
        workdir: &s.work(),
        overlays: &[],
        env: &env,
        command: "quiet command",
        step_env: None,
    };
    let snapshot = crate::data::config::env::EnvSnapshot::with_overrides([(
        "AWMAN_HOME",
        s.awman_home().to_str().unwrap(),
    )]);
    let options = ResolvedContainerOptions::resolve_in(spec.options(), &snapshot).unwrap();
    let plan = backend.plan(options).unwrap();
    assert_eq!(plan.startup_grace, Some(Duration::MAX));
    let expected = (0..250)
        .map(|n| format!("line-{n:03}\n"))
        .collect::<String>();
    plan.output_tail.push_bytes(expected.as_bytes());
    assert_eq!(plan.output_tail.snapshot().len(), 100);
    assert_eq!(plan.output_tail.snapshot_text(), expected);
}

fn backend(s: &awman_test_support::TestSandbox, driver: Arc<Driver>) -> BuiltinBackend {
    BuiltinBackend {
        driver,
        paths: BuiltinPaths {
            home: s.awman_home().to_owned(),
        },
        settings: BuiltinRuntimeSettings {
            state_dir: s.awman_home().to_owned(),
            ambient_overrides: vec![],
            test_isolation: false,
        },
        owner: "test-owner".into(),
        background_env: Mutex::new(Default::default()),
        notices: Mutex::new(vec![]),
        cancel_grace: Duration::ZERO,
    }
}

#[test]
fn explicit_stop_preserves_get_and_remove_checks_current_owner() {
    let s = awman_test_support::TestSandbox::new();
    let driver = Arc::new(Driver::default());
    let backend = backend(&s, driver.clone());
    let name = "awman-profile-owned";
    let labels = naming::labels(
        name,
        "test-owner/generation",
        naming::LaunchLabels {
            vcpus: 1,
            memory_mib: 512,
            network_mode: "none",
        },
        &[],
    )
    .unwrap();
    *driver.0.lock().unwrap() = Some(SandboxSummary {
        handle: AgentHandle {
            id: naming::sandbox_name_for(name),
            name: name.into(),
            image_tag: "fixture".into(),
            started_at: chrono::Utc::now(),
        },
        labels,
        stopped: false,
    });
    let original = driver.list().unwrap()[0].handle.clone();
    assert!(backend.remove_agent(&original.id).is_err());
    backend.stop(&original).unwrap();
    backend.stop(&original).unwrap();
    assert!(driver.get(&original.id).unwrap().stopped);
    assert!(!backend.inspect(&original).unwrap().running);
    assert_eq!(backend.list_stopped().unwrap().len(), 1);
    assert!(driver.remove_stopped(&original.id, "wrong-owner").is_err());
    {
        let mut state = driver.0.lock().unwrap();
        let current = state.as_mut().unwrap();
        current.handle.started_at += chrono::Duration::seconds(1);
        current
            .labels
            .insert(naming::LABEL_OWNER.into(), "test-owner/replacement".into());
        current.stopped = false;
    }
    assert!(matches!(
        backend.stop(&original),
        Err(EngineError::BuiltinAgentReplaced { .. })
    ));
    assert!(!driver.get(&original.id).unwrap().stopped);
    assert!(driver
        .finish_owned(&original.id, "test-owner/generation", Duration::ZERO, true)
        .is_err());
    let replacement = driver.get(&original.id).unwrap().handle;
    backend.stop(&replacement).unwrap();
    assert!(backend
        .remove_agent_checked(&original, "old-execution")
        .is_err());
    assert!(driver.get(&replacement.id).unwrap().stopped);
    backend.remove_agent(&replacement.id).unwrap();
    backend.remove_agent(&replacement.id).unwrap();
    backend.stop(&replacement).unwrap();
    assert!(matches!(
        driver.get(&replacement.id),
        Err(EngineError::AgentNotFound { .. })
    ));
}
