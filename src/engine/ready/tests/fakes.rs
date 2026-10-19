use super::*;
use crate::engine::agent_runtime::frontend::{AgentFrontend, AgentProgress, AgentStatus};
use crate::engine::container::BuildControl;
use crate::engine::image_prep::builder::{BuildRequest, BuildTarget};
pub(crate) struct FakeReadyFrontend {
    pub(crate) create_dockerfile: bool,
    pub(crate) run_audit: bool,
    pub(crate) phases: Vec<ReadyPhase>,
    pub(crate) statuses: Vec<(String, StepStatus)>,
    /// Path passed to the most recent `ask_create_dockerfile` call.
    pub(crate) last_dockerfile_prompt_path: Option<std::path::PathBuf>,
    /// Every `write_message` call, recorded for tests that need to
    /// assert on warning/hint text (e.g. the incompatible-builder
    /// warning, or a build-succeeded/import-failed retry hint).
    pub(crate) messages: Vec<UserMessage>,
}

pub(crate) struct FakeRuntimeFrontend;

impl UserMessageSink for FakeRuntimeFrontend {
    fn write_message(&mut self, _msg: UserMessage) {}
    fn replay_queued(&mut self) {}
}

#[async_trait::async_trait]
impl AgentFrontend for FakeRuntimeFrontend {
    fn report_status(&mut self, _status: AgentStatus) {}
    fn report_progress(&mut self, _progress: AgentProgress) {}
    fn take_io(&mut self) -> crate::engine::agent_runtime::frontend::AgentIo {
        let (stdout_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (stderr_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::engine::agent_runtime::frontend::AgentIo {
            stdout: stdout_tx,
            stderr: stderr_tx,
            stdin_tx,
            stdin_rx,
            resize: None,
            initial_size: None,
        }
    }
}

impl UserMessageSink for FakeReadyFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.messages.push(msg);
    }
    fn replay_queued(&mut self) {}
}

impl FakeReadyFrontend {
    /// Text of every `Warning`-level message written, in order.
    pub(crate) fn warning_texts(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|m| m.level == crate::data::message::MessageLevel::Warning)
            .map(|m| m.text.clone())
            .collect()
    }
}

impl ReadyFrontend for FakeReadyFrontend {
    fn ask_create_dockerfile(
        &mut self,
        dockerfile_path: &std::path::Path,
    ) -> Result<bool, EngineError> {
        self.last_dockerfile_prompt_path = Some(dockerfile_path.to_path_buf());
        Ok(self.create_dockerfile)
    }

    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        Ok(self.run_audit)
    }

    fn report_phase(&mut self, phase: &ReadyPhase) {
        self.phases.push(phase.clone());
    }

    fn report_summary(&mut self, _summary: &ReadySummary) {}
}

impl crate::engine::agent::AgentImageFrontend for FakeReadyFrontend {
    fn report_step_status(&mut self, step: &SetupStep, status: StepStatus) {
        self.statuses.push((step.to_string(), status));
    }

    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        Box::new(FakeRuntimeFrontend)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

pub(crate) struct FakeKitRuntime {
    pub(crate) caps: crate::engine::agent_runtime::Capabilities,
    pub(crate) ready_calls:
        std::sync::Mutex<Vec<(String, crate::engine::agent_runtime::ReadyAgentOptions)>>,
}

impl FakeKitRuntime {
    pub(crate) fn new() -> Self {
        let mut caps = crate::engine::container::ContainerRuntime::docker()
            .capabilities()
            .clone();
        caps.kit_declarative = true;
        caps.has_image_store = false;
        Self {
            caps,
            ready_calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl crate::engine::agent_runtime::AgentRuntimeEngine for FakeKitRuntime {
    fn runtime_name(&self) -> &'static str {
        "fake-kit"
    }
    fn display_name(&self) -> &'static str {
        "Fake Kit Runtime"
    }
    fn capabilities(&self) -> &crate::engine::agent_runtime::Capabilities {
        &self.caps
    }
    fn is_available(&self) -> bool {
        true
    }
    fn build(
        &self,
        _: crate::engine::agent_runtime::ResolvedAgentOptions,
    ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
        unimplemented!("FakeKitRuntime never launches an agent")
    }
    fn list_running(
        &self,
        _: &crate::data::session::Session,
    ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_all(&self) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn stats(
        &self,
        _: &crate::data::session::AgentHandle,
    ) -> Result<crate::engine::agent_runtime::AgentStats, EngineError> {
        Ok(crate::engine::agent_runtime::AgentStats {
            name: String::new(),
            cpu_percent: 0.0,
            memory_mb: 0.0,
        })
    }
    fn stop(&self, _: &crate::data::session::AgentHandle) -> Result<(), EngineError> {
        Ok(())
    }
    fn attach(
        &self,
        _: &crate::data::session::AgentHandle,
    ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
        unimplemented!("FakeKitRuntime never attaches")
    }
    fn list_running_with_name_prefix(
        &self,
        _: &str,
    ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn ready_agent(
        &self,
        agent: &str,
        opts: crate::engine::agent_runtime::ReadyAgentOptions,
        _sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        self.ready_calls
            .lock()
            .unwrap()
            .push((agent.to_string(), opts));
        Ok(())
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Err(EngineError::UnsupportedOnRuntime {
            runtime: "fake-kit",
            operation: "local image probe",
        })
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Err(EngineError::UnsupportedOnRuntime {
            runtime: "fake-kit",
            operation: "image HOME lookup",
        })
    }
    fn build_image(
        &self,
        _request: &crate::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        Err(EngineError::UnsupportedOnRuntime {
            runtime: "fake-kit",
            operation: "image build",
        })
    }
}

pub(crate) struct FakeImportRuntime {
    pub(crate) caps: crate::engine::agent_runtime::Capabilities,
    pub(crate) has_image: bool,
    pub(crate) unavailable: Option<&'static str>,
    pub(crate) cached_source: crate::data::config::image_source::ImageSourceKind,
    pub(crate) cached_request:
        std::sync::Mutex<Option<crate::engine::agent_runtime::ImageImportRequest>>,
    pub(crate) imports: std::sync::Mutex<Vec<crate::engine::agent_runtime::ImageImportRequest>>,
    pub(crate) builds: std::sync::Mutex<usize>,
    pub(crate) wait_for_cancel: bool,
    pub(crate) cancel_observed: std::sync::atomic::AtomicBool,
    /// Overrides the digest `import_image` reports, regardless of the
    /// request's `expected_built_identity`. Used to script a mismatch
    /// between what a local build produced and what import installs.
    pub(crate) digest_override: std::sync::Mutex<Option<char>>,
    /// Number of upcoming `import_image` calls that must fail (counts
    /// down), for "build succeeded, import failed" scenarios.
    pub(crate) fail_next_imports: std::sync::atomic::AtomicUsize,
    /// When set, every `import_image` records how many build-lock files under
    /// this directory are held by someone else at that moment.
    pub(crate) lock_probe_dir: std::sync::Mutex<Option<std::path::PathBuf>>,
    pub(crate) locks_held_at_import: std::sync::Mutex<Vec<usize>>,
}

impl FakeImportRuntime {
    pub(crate) fn new(has_image: bool) -> Self {
        let mut caps = crate::engine::container::ContainerRuntime::docker()
            .capabilities()
            .clone();
        caps.image_acquisition = crate::engine::agent_runtime::ImageAcquisition::Import;
        Self {
            caps,
            has_image,
            unavailable: None,
            cached_source: crate::data::config::image_source::ImageSourceKind::Archive,
            cached_request: std::sync::Mutex::new(None),
            imports: std::sync::Mutex::new(Vec::new()),
            builds: std::sync::Mutex::new(0),
            wait_for_cancel: false,
            cancel_observed: std::sync::atomic::AtomicBool::new(false),
            digest_override: std::sync::Mutex::new(None),
            fail_next_imports: std::sync::atomic::AtomicUsize::new(0),
            lock_probe_dir: std::sync::Mutex::new(None),
            locks_held_at_import: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Make every subsequent `import_image` report `sha256:<c x 64>`,
    /// instead of honouring the request's expected build receipt.
    pub(crate) fn set_digest_override(&self, c: char) {
        *self.digest_override.lock().unwrap() = Some(c);
    }

    /// Make the next `n` `import_image` calls fail.
    pub(crate) fn fail_next_import(&self, n: usize) {
        self.fail_next_imports
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }
}

impl crate::engine::agent_runtime::AgentRuntimeEngine for FakeImportRuntime {
    fn runtime_name(&self) -> &'static str {
        "fake-import"
    }
    fn display_name(&self) -> &'static str {
        "Fake Import Runtime"
    }
    fn capabilities(&self) -> &crate::engine::agent_runtime::Capabilities {
        &self.caps
    }
    fn is_available(&self) -> bool {
        self.unavailable.is_none()
    }
    fn availability(&self) -> Result<(), EngineError> {
        match self.unavailable {
            Some(reason) => Err(EngineError::BuiltinRuntimeUnavailable {
                reason: reason.into(),
            }),
            None => Ok(()),
        }
    }
    fn build(
        &self,
        _: crate::engine::agent_runtime::ResolvedAgentOptions,
    ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
        unimplemented!("FakeImportRuntime never launches an agent")
    }
    fn list_running(
        &self,
        _: &crate::data::session::Session,
    ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn list_running_all(&self) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn stats(
        &self,
        _: &crate::data::session::AgentHandle,
    ) -> Result<crate::engine::agent_runtime::AgentStats, EngineError> {
        unimplemented!()
    }
    fn stop(&self, _: &crate::data::session::AgentHandle) -> Result<(), EngineError> {
        Ok(())
    }
    fn attach(
        &self,
        _: &crate::data::session::AgentHandle,
    ) -> Result<Box<dyn crate::engine::agent_runtime::AgentInstance>, EngineError> {
        unimplemented!()
    }
    fn list_running_with_name_prefix(
        &self,
        _: &str,
    ) -> Result<Vec<crate::data::session::AgentHandle>, EngineError> {
        Ok(vec![])
    }
    fn ready_agent(
        &self,
        _: &str,
        _: crate::engine::agent_runtime::ReadyAgentOptions,
        _: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<(), EngineError> {
        unimplemented!("an importing runtime is not kit-declarative")
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(self.has_image)
    }
    fn image_identity(
        &self,
        tag: &str,
    ) -> Result<Option<crate::data::oci_identity::ImageIdentity>, EngineError> {
        let digest =
            crate::data::oci_identity::Digest::parse(&format!("sha256:{}", "b".repeat(64)))
                .unwrap();
        Ok(self
            .has_image
            .then(|| crate::data::oci_identity::ImageIdentity {
                reference: tag.to_string(),
                manifest_digest: digest.clone(),
                config_digest: digest,
                platform: crate::data::oci_identity::OciPlatform::host_linux(),
                source: self.cached_source,
            }))
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(None)
    }
    fn image_matches_source(
        &self,
        request: &crate::engine::agent_runtime::ImageImportRequest,
    ) -> Result<bool, EngineError> {
        let cached = self.cached_request.lock().unwrap();
        Ok(self.has_image
            && cached.as_ref().is_some_and(|cached| {
                cached.source == request.source
                    && cached.platform == request.platform
                    && request
                        .expected_built_identity
                        .as_ref()
                        .is_none_or(|expected| {
                            self.image_identity(&request.tag)
                                .unwrap()
                                .is_some_and(|identity| expected.matches(&identity))
                        })
            }))
    }
    fn build_image(
        &self,
        _request: &crate::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        *self.builds.lock().unwrap() += 1;
        Err(EngineError::UnsupportedOnRuntime {
            runtime: "fake-import",
            operation: "image build",
        })
    }
    fn import_image_cancellable(
        &self,
        request: &crate::engine::agent_runtime::ImageImportRequest,
        sink: &mut dyn crate::data::message::UserMessageSink,
        cancel: &crate::engine::oci::CancelToken,
    ) -> Result<crate::engine::agent_runtime::ImportedImage, EngineError> {
        if self.wait_for_cancel {
            self.imports.lock().unwrap().push(request.clone());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            self.cancel_observed
                .store(cancel.is_cancelled(), std::sync::atomic::Ordering::SeqCst);
            cancel.check()?;
            return Err(EngineError::Other(
                "ready caller did not cancel import".into(),
            ));
        }
        self.import_image(request, sink)
    }
    fn import_image(
        &self,
        request: &crate::engine::agent_runtime::ImageImportRequest,
        _sink: &mut dyn crate::data::message::UserMessageSink,
    ) -> Result<crate::engine::agent_runtime::ImportedImage, EngineError> {
        self.imports.lock().unwrap().push(request.clone());
        if let Some(dir) = self.lock_probe_dir.lock().unwrap().clone() {
            let held = held_lock_files(&dir);
            self.locks_held_at_import.lock().unwrap().push(held);
        }
        if self
            .fail_next_imports
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n: usize| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(EngineError::Container("fake import failure".into()));
        }
        let hex = match *self.digest_override.lock().unwrap() {
            Some(c) => c.to_string().repeat(64),
            None => match request
                .expected_built_identity
                .as_ref()
                .and_then(|id| id.config_digest().or(id.manifest_digest()))
            {
                Some(digest) => {
                    return Ok(crate::engine::agent_runtime::ImportedImage {
                        tag: request.tag.clone(),
                        identity: crate::data::oci_identity::ImageIdentity {
                            reference: request.tag.clone(),
                            manifest_digest: digest.clone(),
                            config_digest: digest.clone(),
                            platform: request.platform.clone(),
                            source: request.source.kind(),
                        },
                        home_dir: Some("/home/agent".into()),
                        user: None,
                    })
                }
                None => "a".repeat(64),
            },
        };
        let digest = crate::data::oci_identity::Digest::parse(&format!("sha256:{hex}")).unwrap();
        let imported = crate::engine::agent_runtime::ImportedImage {
            tag: request.tag.clone(),
            identity: crate::data::oci_identity::ImageIdentity {
                reference: request.tag.clone(),
                manifest_digest: digest.clone(),
                config_digest: digest,
                platform: request.platform.clone(),
                source: request.source.kind(),
            },
            home_dir: Some("/home/agent".into()),
            user: None,
        };
        crate::engine::oci::CachingAcquirer::check_built_identity(
            request.expected_built_identity.as_ref(),
            &imported.identity,
        )?;
        Ok(imported)
    }
}

#[derive(Default)]
pub(crate) struct FakeLocalBuilder {
    pub(crate) probe_calls: std::sync::atomic::AtomicUsize,
    pub(crate) probe_err: Option<String>,
    pub(crate) inspect_calls: std::sync::Mutex<Vec<String>>,
    pub(crate) inspect_script: std::sync::Mutex<
        std::collections::HashMap<String, std::collections::VecDeque<Result<SourceImage, String>>>,
    >,
    pub(crate) build_calls: std::sync::Mutex<Vec<BuildRequest>>,
    pub(crate) build_script: std::sync::Mutex<
        std::collections::HashMap<
            String,
            std::collections::VecDeque<Result<BuiltImageIdentity, String>>,
        >,
    >,
}

impl FakeLocalBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Every probe/inspect/build call fails with `msg`, as if the
    /// daemon/service were unreachable.
    pub(crate) fn unavailable(msg: impl Into<String>) -> Self {
        Self {
            probe_err: Some(msg.into()),
            ..Self::default()
        }
    }

    pub(crate) fn script_inspect(&self, reference: &str, result: Result<SourceImage, String>) {
        self.inspect_script
            .lock()
            .unwrap()
            .entry(reference.to_string())
            .or_default()
            .push_back(result);
    }

    pub(crate) fn script_build(&self, reference: &str, result: Result<BuiltImageIdentity, String>) {
        self.build_script
            .lock()
            .unwrap()
            .entry(reference.to_string())
            .or_default()
            .push_back(result);
    }

    pub(crate) fn probe_count(&self) -> usize {
        self.probe_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn inspect_count(&self) -> usize {
        self.inspect_calls.lock().unwrap().len()
    }

    pub(crate) fn build_calls(&self) -> Vec<BuildRequest> {
        self.build_calls.lock().unwrap().clone()
    }
}

impl LocalImageBuilder for FakeLocalBuilder {
    fn probe(&self, _target: &BuildTarget, _control: &BuildControl) -> Result<(), EngineError> {
        self.probe_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match &self.probe_err {
            Some(msg) => Err(EngineError::Container(msg.clone())),
            None => Ok(()),
        }
    }

    fn inspect(
        &self,
        _target: &BuildTarget,
        reference: &str,
        _platform: &crate::data::oci_identity::OciPlatform,
        _control: &BuildControl,
    ) -> Result<SourceImage, EngineError> {
        self.inspect_calls
            .lock()
            .unwrap()
            .push(reference.to_string());
        if let Some(err) = &self.probe_err {
            return Err(EngineError::Container(err.clone()));
        }
        let mut script = self.inspect_script.lock().unwrap();
        match script.get_mut(reference).and_then(|q| q.pop_front()) {
            Some(Ok(image)) => Ok(image),
            Some(Err(msg)) => Err(EngineError::Container(msg)),
            None => Ok(SourceImage::Absent),
        }
    }

    fn build(
        &self,
        request: &BuildRequest,
        _control: &BuildControl,
        _on_line: &mut dyn FnMut(&str),
    ) -> Result<BuiltImageIdentity, EngineError> {
        self.build_calls.lock().unwrap().push(request.clone());
        if let Some(err) = &self.probe_err {
            return Err(EngineError::Container(err.clone()));
        }
        let mut script = self.build_script.lock().unwrap();
        match script
            .get_mut(&request.reference)
            .and_then(|q| q.pop_front())
        {
            Some(Ok(id)) => Ok(id),
            Some(Err(msg)) => Err(EngineError::Container(msg)),
            None => Err(EngineError::Container(format!(
                "FakeLocalBuilder: no scripted build result for {}",
                request.reference
            ))),
        }
    }
}

/// How many `*.lock` files under `dir` another holder has locked right now:
/// each is tried once without waiting.
pub(crate) fn held_lock_files(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "lock"))
        .filter(|path| {
            matches!(
                crate::data::fs::private_files::lock_private_timeout(
                    path,
                    crate::data::fs::private_files::LockMode::Exclusive,
                    std::time::Duration::ZERO,
                    || Ok(()),
                ),
                Ok(None)
            )
        })
        .count()
}
