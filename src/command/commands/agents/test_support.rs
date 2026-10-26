//! Shared fakes for the `agents` command tests: a scriptable
//! `AgentRuntimeEngine` that records every call, and a frontend that records
//! every render and prompt through a shared log (so assertions still work
//! after the command consumed the boxed frontend).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use tokio_util::sync::CancellationToken;

use crate::command::commands::agent_setup::{
    AgentLaunchFrontend, AgentSetupDecision, AgentSetupFrontend, HasAgentFrontend,
    NullAgentFrontend,
};
use crate::command::commands::agents::{
    AgentsCommandFrontend, AgentsDetailOutcome, AgentsKillOutcome, AgentsListOutcome,
    AgentsOverview, AgentsTuiContext, KillChoice,
};
use crate::command::commands::mount_scope::{MountScopeDecision, MountScopeFrontend};
use crate::command::error::CommandError;
use crate::data::message::{UserMessage, UserMessageSink};
use crate::data::prompt::Prompt;
use crate::data::session::{AgentHandle, AgentName, Session};
use crate::engine::agent_runtime::execution::{AgentExecution, ExecutionBackend};
use crate::engine::agent_runtime::frontend::AgentFrontend;
use crate::engine::agent_runtime::{
    AgentDetail, AgentExitInfo, AgentHandlePreview, AgentInstance, AgentOwner, AgentRuntimeEngine,
    AgentStats, CancelHandle, Capabilities, ImageAcquisition, ReadyAgentOptions,
    ResolvedAgentOptions, StuckEvent,
};
use crate::engine::error::EngineError;

pub(crate) fn epoch() -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000, 0).single().unwrap()
}

pub(crate) fn handle(id: &str, name: &str, age_secs: i64) -> AgentHandle {
    AgentHandle {
        id: id.to_string(),
        image_tag: "awman-test:latest".to_string(),
        name: name.to_string(),
        started_at: Utc::now() - chrono::Duration::seconds(age_secs),
    }
}

/// Everything the fake runtime was asked to do, in order.
pub(crate) type CallLog = Arc<Mutex<Vec<String>>>;

pub(crate) struct FakeRuntime {
    pub caps: Capabilities,
    pub available: Mutex<Result<(), String>>,
    pub running: Mutex<Vec<AgentHandle>>,
    pub stopped: Mutex<Vec<AgentHandle>>,
    /// Names whose `stats()` call fails.
    pub stats_fail: Mutex<Vec<String>>,
    pub stats_value: Mutex<(f64, f64)>,
    pub stats_gate: Option<Arc<AtomicBool>>,
    pub reattach: bool,
    pub inspect_override: Mutex<Option<AgentDetail>>,
    pub stop_error: Mutex<Option<EngineError>>,
    pub remove_result: Mutex<Option<Result<(), EngineError>>>,
    /// Exit code the fake attach session reports.
    pub attach_exit: i32,
    /// When true the attach session blocks until its cancel handle fires.
    pub attach_blocks: bool,
    pub calls: CallLog,
    pub detach_cancels: Arc<AtomicUsize>,
    pub availability_checks: AtomicUsize,
    /// A failing `stop` also drops the agent from the running list.
    pub vanish_on_stop_error: AtomicBool,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self {
            caps: Capabilities {
                arbitrary_host_mounts: true,
                kit_declarative: false,
                has_image_store: true,
                image_acquisition: ImageAcquisition::Build,
                per_launch_vm_settings: false,
                reports_resource_stats: true,
            },
            available: Mutex::new(Ok(())),
            running: Mutex::new(Vec::new()),
            stopped: Mutex::new(Vec::new()),
            stats_fail: Mutex::new(Vec::new()),
            stats_value: Mutex::new((10.0, 100.0)),
            stats_gate: None,
            reattach: true,
            inspect_override: Mutex::new(None),
            stop_error: Mutex::new(None),
            remove_result: Mutex::new(None),
            attach_exit: 0,
            attach_blocks: false,
            calls: Arc::new(Mutex::new(Vec::new())),
            detach_cancels: Arc::new(AtomicUsize::new(0)),
            availability_checks: AtomicUsize::new(0),
            vanish_on_stop_error: AtomicBool::new(false),
        }
    }

    pub fn with_running(self, handles: Vec<AgentHandle>) -> Self {
        *self.running.lock().unwrap() = handles;
        self
    }

    pub fn with_stopped(self, handles: Vec<AgentHandle>) -> Self {
        *self.stopped.lock().unwrap() = handles;
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn called(&self, prefix: &str) -> bool {
        self.calls().iter().any(|c| c.starts_with(prefix))
    }

    fn record(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }
}

struct FakeAttachInstance {
    handle: AgentHandle,
    exit_code: i32,
    blocks: bool,
    detach_cancels: Arc<AtomicUsize>,
}

struct FakeBackend {
    exit_code: i32,
    blocks: bool,
    cancelled: Arc<AtomicBool>,
    detach_cancels: Arc<AtomicUsize>,
}

impl ExecutionBackend for FakeBackend {
    fn wait_blocking(self: Box<Self>) -> Result<AgentExitInfo, EngineError> {
        while self.blocks && !self.cancelled.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(AgentExitInfo {
            exit_code: self.exit_code,
            signal: None,
            started_at: epoch(),
            ended_at: epoch(),
        })
    }

    fn cancel(&self) -> Result<(), EngineError> {
        self.cancelled.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn cancel_handle(&self) -> CancelHandle {
        let cancelled = self.cancelled.clone();
        let counter = self.detach_cancels.clone();
        CancelHandle::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            cancelled.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

impl AgentInstance for FakeAttachInstance {
    fn handle_preview(&self) -> AgentHandlePreview {
        AgentHandlePreview {
            id: self.handle.id.clone(),
            name: self.handle.name.clone(),
            image: self.handle.image_tag.clone(),
        }
    }

    fn run_with_frontend(
        self: Box<Self>,
        _frontend: Box<dyn AgentFrontend>,
    ) -> Result<AgentExecution, EngineError> {
        let (tx, _) = tokio::sync::broadcast::channel::<StuckEvent>(4);
        let backend = FakeBackend {
            exit_code: self.exit_code,
            blocks: self.blocks,
            cancelled: Arc::new(AtomicBool::new(false)),
            detach_cancels: self.detach_cancels.clone(),
        };
        Ok(AgentExecution::new(
            self.handle.clone(),
            Box::new(backend),
            Arc::new(tx),
            None,
        ))
    }
}

impl AgentRuntimeEngine for FakeRuntime {
    fn runtime_name(&self) -> &'static str {
        "fake"
    }
    fn display_name(&self) -> &'static str {
        "Fake Runtime"
    }
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    fn is_available(&self) -> bool {
        self.available.lock().unwrap().is_ok()
    }
    fn availability(&self) -> Result<(), EngineError> {
        self.availability_checks.fetch_add(1, Ordering::SeqCst);
        self.available
            .lock()
            .unwrap()
            .clone()
            .map_err(EngineError::Other)
    }
    fn build(&self, _: ResolvedAgentOptions) -> Result<Box<dyn AgentInstance>, EngineError> {
        unimplemented!("agents commands never build")
    }
    fn list_running(&self, _: &Session) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(self.running.lock().unwrap().clone())
    }
    fn list_running_all(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.record("list_running_all");
        Ok(self.running.lock().unwrap().clone())
    }
    fn stats(&self, handle: &AgentHandle) -> Result<AgentStats, EngineError> {
        self.record(format!("stats {}", handle.name));
        if let Some(gate) = &self.stats_gate {
            while !gate.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        if self.stats_fail.lock().unwrap().contains(&handle.name) {
            return Err(EngineError::Other("stats failed".into()));
        }
        let (cpu, mem) = *self.stats_value.lock().unwrap();
        Ok(AgentStats {
            name: handle.name.clone(),
            cpu_percent: cpu,
            memory_mb: mem,
        })
    }
    fn stop(&self, handle: &AgentHandle) -> Result<(), EngineError> {
        self.record(format!("stop {}", handle.id));
        if let Some(e) = self.stop_error.lock().unwrap().take() {
            if self.vanish_on_stop_error.load(Ordering::SeqCst) {
                self.running.lock().unwrap().retain(|h| h.id != handle.id);
            }
            return Err(e);
        }
        self.running.lock().unwrap().retain(|h| h.id != handle.id);
        Ok(())
    }
    fn attach(&self, handle: &AgentHandle) -> Result<Box<dyn AgentInstance>, EngineError> {
        self.record(format!("attach {}", handle.id));
        Ok(Box::new(FakeAttachInstance {
            handle: handle.clone(),
            exit_code: self.attach_exit,
            blocks: self.attach_blocks,
            detach_cancels: self.detach_cancels.clone(),
        }))
    }
    fn list_running_with_name_prefix(&self, _: &str) -> Result<Vec<AgentHandle>, EngineError> {
        Ok(Vec::new())
    }
    fn list_stopped(&self) -> Result<Vec<AgentHandle>, EngineError> {
        self.record("list_stopped");
        Ok(self.stopped.lock().unwrap().clone())
    }
    fn inspect(&self, handle: &AgentHandle) -> Result<AgentDetail, EngineError> {
        self.record(format!("inspect {}", handle.id));
        if let Some(detail) = self.inspect_override.lock().unwrap().clone() {
            return Ok(detail);
        }
        let reports = self.caps.reports_resource_stats;
        let mut detail = crate::engine::agent_runtime::execution::default_detail(
            reports,
            handle,
            self.stats(handle),
        );
        detail.running = self
            .running
            .lock()
            .unwrap()
            .iter()
            .any(|h| h.id == handle.id);
        Ok(detail)
    }
    fn reattach_after_owner_exit(&self) -> bool {
        self.reattach
    }
    fn remove_agent(&self, id: &str) -> Result<(), EngineError> {
        self.record(format!("remove {id}"));
        match self.remove_result.lock().unwrap().take() {
            Some(result) => result,
            None => Err(EngineError::UnsupportedOnRuntime {
                runtime: "fake",
                operation: "agent removal",
            }),
        }
    }
    fn ready_agent(
        &self,
        _: &str,
        _: ReadyAgentOptions,
        _: &mut dyn UserMessageSink,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn image_exists(&self, _: &str) -> Result<bool, EngineError> {
        Ok(true)
    }
    fn image_home_dir(&self, _: &str) -> Result<Option<String>, EngineError> {
        Ok(None)
    }
    fn build_image(
        &self,
        _: &crate::engine::container::BuildRequest,
        _: &mut dyn FnMut(&str),
    ) -> Result<(), EngineError> {
        Ok(())
    }
}

pub(crate) fn owner_detail(handle: &AgentHandle, pid: u32, alive: bool) -> AgentDetail {
    let mut detail = crate::engine::agent_runtime::execution::default_detail(
        false,
        handle,
        Err(EngineError::Other("n/a".into())),
    );
    detail.owner = Some(AgentOwner { pid, alive });
    detail
}

/// What the fake frontend saw.
#[derive(Default)]
pub(crate) struct FrontendLog {
    /// `(agent count, watching)` per `render_overview`.
    pub renders: Vec<(usize, bool, String)>,
    pub begin_watch: usize,
    pub end_watch: usize,
    pub kill_prompts: Vec<Prompt<KillChoice>>,
    pub list_outcomes: usize,
    pub detail_outcomes: usize,
    pub kill_outcomes: Vec<(String, bool)>,
    pub messages: Vec<UserMessage>,
    pub pty_active: Vec<bool>,
    pub stuck_senders: usize,
    pub container_frontends: usize,
}

pub(crate) struct FakeFrontend {
    pub log: Arc<Mutex<FrontendLog>>,
    pub tui: Option<AgentsTuiContext>,
    pub watch_token: CancellationToken,
    pub attach_token: CancellationToken,
    /// Cancel the watch token after this many renders.
    pub cancel_after_renders: Option<usize>,
    /// `None` → the trait default (non-interactive refusal).
    pub kill_answer: Option<KillChoice>,
}

impl FakeFrontend {
    pub fn new() -> (Self, Arc<Mutex<FrontendLog>>) {
        let log = Arc::new(Mutex::new(FrontendLog::default()));
        (
            Self {
                log: log.clone(),
                tui: None,
                watch_token: CancellationToken::new(),
                attach_token: CancellationToken::new(),
                cancel_after_renders: None,
                kill_answer: None,
            },
            log,
        )
    }

    pub fn boxed(self) -> Box<dyn AgentsCommandFrontend> {
        Box::new(self)
    }
}

impl UserMessageSink for FakeFrontend {
    fn write_message(&mut self, msg: UserMessage) {
        self.log.lock().unwrap().messages.push(msg);
    }
    fn replay_queued(&mut self) {}
}

impl MountScopeFrontend for FakeFrontend {
    fn ask_mount_scope(
        &mut self,
        _: &std::path::Path,
        _: &std::path::Path,
    ) -> Result<MountScopeDecision, CommandError> {
        Ok(MountScopeDecision::MountGitRoot)
    }
}

impl AgentSetupFrontend for FakeFrontend {
    fn ask_agent_setup(
        &mut self,
        _: &AgentName,
        _: &AgentName,
        _: bool,
        _: bool,
    ) -> Result<AgentSetupDecision, CommandError> {
        Ok(AgentSetupDecision::Setup)
    }
    fn record_fallback(&mut self, _: &AgentName, _: &AgentName) {}
}

impl crate::command::commands::agent_auth::AgentAuthFrontend for FakeFrontend {
    fn ask_agent_auth_consent(
        &mut self,
        _: &AgentName,
        _: &[&str],
    ) -> Result<crate::command::commands::agent_auth::AgentAuthDecision, CommandError> {
        Ok(crate::command::commands::agent_auth::AgentAuthDecision::Decline)
    }
}

impl HasAgentFrontend for FakeFrontend {
    fn container_frontend(&mut self) -> Box<dyn AgentFrontend> {
        self.log.lock().unwrap().container_frontends += 1;
        Box::new(NullAgentFrontend)
    }
}

impl crate::command::commands::agent_trust::AgentTrustFrontend for FakeFrontend {}
impl AgentLaunchFrontend for FakeFrontend {
    fn set_pty_active(&mut self, active: bool) {
        self.log.lock().unwrap().pty_active.push(active);
    }
    fn set_stuck_sender(&mut self, _: Arc<tokio::sync::broadcast::Sender<StuckEvent>>) {
        self.log.lock().unwrap().stuck_senders += 1;
    }
}

impl AgentsCommandFrontend for FakeFrontend {
    fn tui_context(&self) -> Option<AgentsTuiContext> {
        self.tui.clone()
    }
    fn begin_watch(&mut self) -> CancellationToken {
        self.log.lock().unwrap().begin_watch += 1;
        self.watch_token.clone()
    }
    fn render_overview(&mut self, overview: &AgentsOverview, watching: bool) {
        let renders = {
            let mut log = self.log.lock().unwrap();
            log.renders
                .push((overview.agents.len(), watching, overview.tip.clone()));
            log.renders.len()
        };
        if self.cancel_after_renders == Some(renders) {
            self.watch_token.cancel();
        }
    }
    fn end_watch(&mut self) {
        self.log.lock().unwrap().end_watch += 1;
    }
    fn show_list(&mut self, _: &AgentsListOutcome) {
        self.log.lock().unwrap().list_outcomes += 1;
    }
    fn show_detail(&mut self, _: &AgentsDetailOutcome) {
        self.log.lock().unwrap().detail_outcomes += 1;
    }
    fn show_kill_result(&mut self, outcome: &AgentsKillOutcome) {
        self.log
            .lock()
            .unwrap()
            .kill_outcomes
            .push((outcome.id.clone(), outcome.removed));
    }
    fn confirm_kill(&mut self, prompt: &Prompt<KillChoice>) -> Result<KillChoice, CommandError> {
        self.log.lock().unwrap().kill_prompts.push(prompt.clone());
        match self.kill_answer {
            Some(choice) => Ok(choice),
            None => Err(CommandError::InteractiveInputUnavailable {
                prompt: "yes".into(),
            }),
        }
    }
    fn attach_cancel(&self) -> CancellationToken {
        self.attach_token.clone()
    }
}
