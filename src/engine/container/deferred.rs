//! Selected runtime whose transient startup failure is retried by observation calls.
use std::collections::HashMap;

use crate::data::message::UserMessageSink;
use crate::data::oci_identity::ImageIdentity;
use crate::data::session::{AgentHandle, Session};
use crate::engine::agent_runtime::background::ExecOutput;
use crate::engine::agent_runtime::execution::{AgentDetail, AgentInstance, AgentStats};
use crate::engine::agent_runtime::{Capabilities, ImageImportRequest, ImportedImage};
use crate::engine::container::background::BackgroundLaunchRequest;
use crate::engine::container::options::ResolvedContainerOptions;
use crate::engine::container::runtime::ContainerImageInfo;
use crate::engine::error::EngineError;

use super::backend::ContainerBackend;
use std::sync::{Arc, Mutex};

pub(super) type OpenBackend =
    dyn Fn() -> Result<Arc<dyn ContainerBackend>, EngineError> + Send + Sync;

pub(super) struct DeferredBackend {
    open: Box<OpenBackend>,
    backend: Mutex<Option<Arc<dyn ContainerBackend>>>,
}

static CAPABILITIES: Capabilities = Capabilities {
    arbitrary_host_mounts: true,
    kit_declarative: false,
    has_image_store: true,
    image_acquisition: crate::engine::agent_runtime::ImageAcquisition::Import,
    per_launch_vm_settings: true,
    reports_resource_stats: true,
};

impl DeferredBackend {
    pub(super) fn new(open: Box<OpenBackend>) -> Self {
        Self {
            open,
            backend: Mutex::new(None),
        }
    }

    fn get(&self) -> Result<Arc<dyn ContainerBackend>, EngineError> {
        let mut cached = self.backend.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(backend) = cached.as_ref() {
            return Ok(backend.clone());
        }
        let backend = (self.open)()?;
        *cached = Some(backend.clone());
        Ok(backend)
    }
}

impl ContainerBackend for DeferredBackend {
    fn name(&self) -> &'static str {
        "builtin"
    }
    fn display_name(&self) -> &'static str {
        "Builtin microVM (experimental)"
    }
    fn capabilities(&self) -> &'static Capabilities {
        &CAPABILITIES
    }
    fn reattach_after_owner_exit(&self) -> bool {
        false
    }
    fn is_available(&self) -> Result<(), EngineError> {
        self.get()?.is_available()
    }
    fn take_notices(&self) -> Vec<String> {
        // Never forces an open: only a backend that already opened has any.
        let cached = self.backend.lock().unwrap_or_else(|e| e.into_inner());
        cached
            .as_ref()
            .map(|backend| backend.take_notices())
            .unwrap_or_default()
    }
    fn build(
        &self,
        options: ResolvedContainerOptions,
    ) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.get()?.build(options)
    }
    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.get()?.attach(handle)
    }
    fn list_running(&self, session: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        self.get()?.list_running(session)
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.get()?.list_running_all()
    }
    fn list_running_with_name_prefix(&self, prefix: &str) -> Result<Vec<AgentHandle>, EngineError> {
        self.get()?.list_running_with_name_prefix(prefix)
    }
    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.get()?.list_stopped()
    }
    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.get()?.stats(handle)
    }
    fn inspect(&self, handle: &AgentHandle) -> Result<AgentDetail, EngineError> {
        self.get()?.inspect(handle)
    }
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.get()?.stop(handle)
    }
    fn remove_agent(&self, id: &str) -> Result<(), EngineError> {
        self.get()?.remove_agent(id)
    }
    fn run_probe(&self, request: &super::backend::ProbeRequest<'_>) -> Result<String, EngineError> {
        self.get()?.run_probe(request)
    }
    fn image_id(&self, tag: &str) -> Result<Option<String>, EngineError> {
        self.get()?.image_id(tag)
    }
    fn image_exists(&self, tag: &str) -> Result<bool, EngineError> {
        self.get()?.image_exists(tag)
    }
    fn image_home_dir(&self, tag: &str) -> Result<Option<String>, EngineError> {
        self.get()?.image_home_dir(tag)
    }
    fn image_identity(&self, _tag: &str) -> Result<Option<ImageIdentity>, EngineError> {
        self.get()?.image_identity(_tag)
    }
    fn image_matches_source(&self, _request: &ImageImportRequest) -> Result<bool, EngineError> {
        self.get()?.image_matches_source(_request)
    }
    fn build_image(
        &self,
        request: &crate::engine::container::BuildRequest,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        self.get()?.build_image(request, on_line)
    }
    fn import_image(
        &self,
        request: &ImageImportRequest,
        sink: &mut dyn UserMessageSink,
    ) -> Result<ImportedImage, EngineError> {
        self.get()?.import_image(request, sink)
    }
    fn import_image_cancellable(
        &self,
        request: &ImageImportRequest,
        sink: &mut dyn crate::data::message::UserMessageSink,
        cancel: &crate::engine::oci::CancelToken,
    ) -> Result<ImportedImage, EngineError> {
        self.get()?.import_image_cancellable(request, sink, cancel)
    }
    fn list_dangling_images(&self) -> Result<Vec<ContainerImageInfo>, EngineError> {
        self.get()?.list_dangling_images()
    }
    fn remove_image(&self, id: &str) -> Result<(), EngineError> {
        self.get()?.remove_image(id)
    }
    fn start_background(
        &self,
        request: &BackgroundLaunchRequest<'_>,
    ) -> Result<String, EngineError> {
        self.get()?.start_background(request)
    }
    fn exec_in_background(
        &self,
        id: &str,
        command: &str,
        working_dir: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<ExecOutput, EngineError> {
        self.get()?
            .exec_in_background(id, command, working_dir, env)
    }
    fn exec_in_background_streaming(
        &self,
        request: &crate::engine::container::background::BackgroundExecRequest<'_>,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<ExecOutput, EngineError> {
        self.get()?.exec_in_background_streaming(request, on_line)
    }
    fn stop_and_remove(&self, id: &str) -> Result<(), EngineError> {
        self.get()?.stop_and_remove(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn fatal_builtin_configuration_errors_are_not_deferred() {
        let result = super::super::runtime::ContainerRuntime::open_builtin(Box::new(|| {
            Err(EngineError::Config("invalid builtin configuration".into()))
        }));
        assert!(matches!(result, Err(EngineError::Config(_))));
    }

    #[test]
    fn startup_unavailability_is_retried_without_switching_runtime() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();
        let runtime = super::super::runtime::ContainerRuntime::open_builtin(Box::new(move || {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(EngineError::BuiltinRuntimeUnavailable {
                    reason: "starting".into(),
                })
            } else {
                Ok(Arc::new(
                    super::super::test_support::HoldingBackend::default(),
                ))
            }
        }))
        .expect("recoverable construction failure retains a runtime");
        assert_eq!(runtime.runtime_name(), "builtin");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "construction was attempted"
        );
        assert!(runtime.availability().is_ok());
        assert!(runtime.availability().is_ok());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "cache successful construction"
        );
    }

    #[test]
    fn a_notice_from_a_deferred_open_is_returned_once_after_it_opens() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();
        let runtime = super::super::runtime::ContainerRuntime::open_builtin(Box::new(move || {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(EngineError::BuiltinRuntimeUnavailable {
                    reason: "starting".into(),
                });
            }
            let backend = super::super::test_support::HoldingBackend::default();
            backend.notices.lock().unwrap().push("upgraded".into());
            Ok(Arc::new(backend))
        }))
        .unwrap();
        assert!(
            runtime.take_notices().is_empty(),
            "draining never forces an open"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(runtime.availability().is_ok());
        assert_eq!(runtime.take_notices(), vec!["upgraded".to_string()]);
        assert!(runtime.take_notices().is_empty());
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn builtin_construction_under_isolation_retains_selected_runtime() {
        let state = tempfile::tempdir().unwrap();
        let runtime = super::super::runtime::ContainerRuntime::builtin(
            crate::engine::container::runtime::BuiltinRuntimeSettings {
                state_dir: state.path().to_path_buf(),
                ambient_overrides: vec![],
                test_isolation: true,
            },
        )
        .expect("transient startup unavailability must leave an observable runtime");
        assert_eq!(runtime.runtime_name(), "builtin");
        assert!(matches!(
            runtime.availability(),
            Err(EngineError::BuiltinRuntimeUnavailable { .. })
        ));
    }
}
