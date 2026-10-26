//! Daemon-free WI0130 fixtures shared by engine and command integration tests.
#![allow(dead_code)]
use std::path::Path;
use std::sync::{Arc, Mutex};

use awman::data::message::{UserMessage, UserMessageSink};
use awman::data::oci_identity::{BuiltImageIdentity, Digest, OciPlatform};
use awman::data::session::{AgentName, Session};
use awman::engine::agent::{AgentEngine, AgentImageFrontend, AgentRegistry};
use awman::engine::agent_runtime::*;
use awman::engine::container::{BuildControl, BuildRequest, BuildTarget, ContainerRuntime};
use awman::engine::error::EngineError;
use awman::engine::image_prep::builder::{LocalImageBuilder, SourceImage};
use awman::engine::overlay::OverlayEngine;
use awman::engine::ready::{
    ReadyEngine, ReadyEngineOptions, ReadyFrontend, ReadyPhase, ReadySummary,
};

pub const INSTALL: &str = r#"schema = 1
display_name = "Pi"
[run]
command = ["pi", "--interactive"]
non_interactive = "run"
model = "equals"
env = { PI_STATIC = "fixture" }
[modes]
yolo = ["--reckless"]
auto = ["--automatic"]
plan = ["--planning"]
[system_prompt]
delivery = "append-inline"
flag = "--instructions"
key = "instructions"
[settings]
host_dir = ".pi"
skills_dir = ".pi/skills"
[auth]
env = ["WI0130_TOKEN"]
[image]
install = ["printf pi-v1 > /usr/local/bin/pi", "chmod +x /usr/local/bin/pi"]
"#;

pub fn write_definition(root: &Path, name: &str, content: &str) {
    let dir = root.join(".awman/agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.toml")), content).unwrap();
}

pub fn approve(session: &Session, name: &str) {
    let name = AgentName::new(name).unwrap();
    let def = AgentRegistry::load_untrusted(session.env(), &name, Some(session.definition_root()))
        .unwrap();
    AgentRegistry::record_approval(
        session.env(),
        def.definition(),
        Some(session.definition_root()),
    )
    .unwrap();
}

#[derive(Default)]
pub struct RecordingBuilder {
    pub builds: Mutex<Vec<(BuildRequest, String)>>,
}
impl LocalImageBuilder for RecordingBuilder {
    fn probe(&self, _: &BuildTarget, _: &BuildControl) -> Result<(), EngineError> {
        Ok(())
    }
    fn inspect(
        &self,
        _: &BuildTarget,
        _: &str,
        _: &OciPlatform,
        _: &BuildControl,
    ) -> Result<SourceImage, EngineError> {
        panic!("building-runtime ready must not inspect a source store")
    }
    fn build(
        &self,
        request: &BuildRequest,
        _: &BuildControl,
        _: &mut dyn FnMut(&str),
    ) -> Result<BuiltImageIdentity, EngineError> {
        self.builds.lock().unwrap().push((
            request.clone(),
            std::fs::read_to_string(&request.dockerfile).unwrap(),
        ));
        Ok(BuiltImageIdentity::new(
            Some(Digest::parse(&format!("sha256:{}", "a".repeat(64))).unwrap()),
            None,
            request.platform.clone(),
        )
        .unwrap())
    }
}

/// All images already exist: any recorded build was forced by the definition,
/// rather than accidentally caused by a missing image. No instance is started.
pub struct RecordingRuntime {
    pub builder: RecordingBuilder,
    pub options: Mutex<Vec<ResolvedAgentOptions>>,
    caps: Capabilities,
}
impl Default for RecordingRuntime {
    fn default() -> Self {
        Self {
            builder: Default::default(),
            options: Default::default(),
            caps: ContainerRuntime::docker().capabilities().clone(),
        }
    }
}
impl RecordingRuntime {
    pub fn sandbox() -> Self {
        let mut result = Self::default();
        result.caps.kit_declarative = true;
        result.caps.image_acquisition = ImageAcquisition::Kit;
        result.caps.arbitrary_host_mounts = false;
        result
    }
}
impl AgentRuntimeEngine for RecordingRuntime {
    fn runtime_name(&self) -> &'static str {
        if self.caps.kit_declarative {
            "docker-sbx-experimental"
        } else {
            "recording"
        }
    }
    fn display_name(&self) -> &'static str {
        self.runtime_name()
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    fn is_available(&self) -> bool {
        true
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(Some("/root".into()))
    }
    fn build_image(
        &self,
        request: &BuildRequest,
        sink: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        self.builder
            .build(request, &BuildControl::unbounded(Default::default()), sink)
            .map(|_| ())
    }
    fn build(&self, options: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.options.lock().unwrap().push(options);
        Err(EngineError::Other("WI0130 recorded launch".into()))
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("unexpected list")
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("unexpected list")
    }
    fn stats(&self, _: &AgentHandle) -> Result<AgentStats, EngineError> {
        panic!("unexpected stats")
    }
    fn stop(&self, _: &AgentHandle) -> Result<(), EngineError> {
        panic!("unexpected stop")
    }
    fn attach(&self, _: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        panic!("unexpected attach")
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        panic!("unexpected list")
    }
    fn ready_agent(
        &self,
        _: &str,
        _: ReadyAgentOptions,
        _: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        panic!("custom agent must be refused before kit emission")
    }
}

#[derive(Default)]
pub struct ReadyRecorder {
    pub statuses: Vec<(
        awman::data::setup_step::SetupStep,
        awman::data::step_status::StepStatus,
    )>,
    pub messages: Vec<UserMessage>,
}
impl UserMessageSink for ReadyRecorder {
    fn write_message(&mut self, message: UserMessage) {
        self.messages.push(message);
    }
    fn replay_queued(&mut self) {}
}
impl AgentImageFrontend for ReadyRecorder {
    fn report_step_status(
        &mut self,
        step: &awman::data::setup_step::SetupStep,
        status: awman::data::step_status::StepStatus,
    ) {
        self.statuses.push((step.clone(), status));
    }
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        panic!("unexpected container")
    }
}
impl ReadyFrontend for ReadyRecorder {
    fn ask_create_dockerfile(&mut self, _: &Path) -> Result<bool, EngineError> {
        panic!("fixture has Dockerfile.dev")
    }
    fn ask_run_audit_on_template(&mut self) -> Result<bool, EngineError> {
        panic!("unexpected prompt")
    }
    fn report_phase(&mut self, _: &ReadyPhase) {}
    fn report_summary(&mut self, _: &ReadySummary) {}
}

pub fn ready(
    session: Arc<Session>,
    runtime: Arc<dyn AgentRuntimeEngine>,
    agent: &str,
) -> ReadyEngine {
    let home = awman::data::config::GlobalConfig::data_home_with(session.env()).unwrap();
    let overlay = Arc::new(OverlayEngine::with_auth_resolver(
        awman::data::fs::AuthPathResolver::at_home(&home),
    ));
    let engine = Arc::new(AgentEngine::new(overlay.clone(), runtime.clone()));
    ReadyEngine::new(
        None,
        session.git_root().join("data"),
        session,
        Arc::new(awman::engine::git::GitEngine::new()),
        overlay,
        runtime,
        engine,
        ReadyEngineOptions {
            agent: AgentName::new(agent).unwrap(),
            refresh: false,
            build: false,
            no_cache: false,
            allow_docker: false,
            non_interactive: true,
            env_passthrough: None,
        },
    )
}

/// Identical synthetic custom agent used by Docker, Apple and builtin guests.
/// It has no provider, host command, credentials or network dependency.
pub const RUNTIME_SMOKE: &str = r#"schema = 1
display_name = "Pi runtime smoke"
[run]
command = ["/bin/sh", "-c", "printf WI0130_CUSTOM_AGENT; exit 37"]
[image]
install = ["true"]
"#;
