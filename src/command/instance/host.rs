//! `EngineInstanceHost`: the daemon-side owner of every hosted instance
//! (WI 0142 Part A §1–§4, contract §4.8).
//!
//! The host owns the sessions (through each [`EngineInstance`]'s
//! `SessionManager`), Dispatch and the command threads (through the
//! instance), the dialogs (first answer wins), frontend admission and the exit
//! policy. It exposes **typed operations** — [`EngineInstanceHost::create_instance`],
//! [`list`](EngineInstanceHost::list), [`get`](EngineInstanceHost::get),
//! [`delete`](EngineInstanceHost::delete), annotations, tabs, dialogs,
//! [`ui_request`](EngineInstanceHost::ui_request) and
//! [`attach_frontend`](EngineInstanceHost::attach_frontend) — that Layer 3
//! maps to HTTP and the `awman.frontend.v1` WebSocket. It constructs no route
//! and frames nothing beyond the Layer 0 [`EngineFrame`]s it hands out.
//!
//! It implements the Layer 1 [`EngineHost`] hook, so the `engine` feature's
//! idle clock, drain and stop reach it without knowing what an instance is.
//!
//! Security (§8): each instance's `EnvSnapshot` (the primary `Hello.env`) is
//! held in memory only, inside the instance, and dropped when it ends. It is
//! never logged (its `Debug` is redacted, and nothing here logs it), never
//! put in an event, a frame or the store.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::command::commands::agents::AgentsTuiContext;
use crate::command::commands::squad::indicator::EnvSource;
use crate::command::dispatch::catalogue::{CommandCatalogue, FrontendKind};
use crate::command::dispatch::parsed_input::ParsedCommandBoxInput;
use crate::command::dispatch::Dispatch;
use crate::command::error::CommandError;
use crate::command::integrations::presenter::{call_ui_request, PRESENTER_TIMEOUT};
use crate::command::integrations::{
    AnnotationWriter, HostPresenter, IntegrationRegistry, NoticeSink, PresenterEvent,
    PresenterSupervisor,
};
use crate::command::stats_sampler::{ContainerStatsSampler, StatsRequest, StatsTarget};
use crate::data::annotations::{AnnotationError, AnnotationFilter, Annotations};
use crate::data::config::daemon::EngineSettings;
use crate::data::config::env::EnvSnapshot;
use crate::data::config::ConfigOverrides;
use crate::data::daemon_state::FeatureStatus;
use crate::data::dialog::DialogResponse;
use crate::data::frontend_protocol::{
    chunk_snapshot, AnsweredBy, CommandResultWire, EngineFrame, ErrorWire, FrontendFrame,
    FrontendKindWire, FrontendRole, Hello, OpenTabOptions, ProtocolError, Seq, SubmitInput,
    TabKindWire, TermSize, UiOutcome, UiRequest, FRONTEND_SUBPROTOCOL,
};
use crate::data::ids::{DialogId, FrontendId, InstanceId, TabId, TerminalId};
use crate::data::instance_snapshot::{
    AnnotationTarget, DialogAnswerOutcome, FrontendSummary, InstanceDelta, InstanceDetail,
    InstanceSnapshot, InstanceSummary, PendingDialog, SquadHealthWire, TabSnapshot,
};
use crate::data::message::MessageLevel;
use crate::data::session::SessionOpenOptions;
use crate::data::session_manager::SessionManager;
use crate::data::wire::{Cursor, List, Page};
use crate::engine::daemon::executor::SharedExecutor;
use crate::engine::daemon::features::engine::{EngineActivity, EngineHost, EngineStopReason};
use crate::engine::terminal_hub::TerminalHub;

use super::admission::{admit, Admitted, AttachRefusal};
use super::frontend::InstancePresentation;
use super::lifecycle::{FrontendLifecycle, LifecycleAction, QuitReason};
use super::relay::{InstanceEvent, TabRelay};
use super::{
    EngineInstance, HostedOptions, InitialTabOpened, InitialTabSpec, InstanceParts, OpenTabRequest,
    OpenedTab, QuitPlan, StartupRun, SubmitRequest,
};

/// How often the host pumps every tab relay and advances lifecycle timers.
pub const PUMP_INTERVAL: Duration = Duration::from_millis(50);

/// Answered dialog ids remembered per instance, so a late answer gets
/// `AlreadyAnswered { by }` rather than `not found`.
const ANSWERED_MEMORY: usize = 64;

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Why a typed host operation failed. Layer 3 maps [`HostError::code`] to its
/// `{error, code}` body.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// 404 `not_found`.
    #[error("instance not found")]
    NotFound,
    /// 404 `not_found`.
    #[error("tab not found")]
    UnknownTab,
    /// 404 `not_found`.
    #[error("dialog not found")]
    UnknownDialog,
    #[error("directory is outside the launch contexts")]
    DirOutsideContext,
    /// 400 `invalid_annotation`.
    #[error(transparent)]
    InvalidAnnotation(#[from] AnnotationError),
    /// 503 `feature_disabled`.
    #[error("engine is draining")]
    Draining,
    #[error(transparent)]
    Command(#[from] CommandError),
}

impl HostError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound | Self::UnknownTab | Self::UnknownDialog => "not_found",
            Self::InvalidAnnotation(_) => AnnotationError::CODE,
            Self::Draining => "feature_disabled",
            Self::DirOutsideContext => "dir_outside_context",
            Self::Command(error) => error.code(),
        }
    }
}

// ─── Opening instances ──────────────────────────────────────────────────────

/// What the host hands an [`InstanceOpener`] when a primary `Hello` arrives.
pub struct OpenInstanceRequest {
    pub kind: FrontendKindWire,
    pub input_available: bool,
    pub id: InstanceId,
    /// The primary's whole environment, cwd and argv (memory only).
    pub env: EnvSnapshot,
    /// The agent-window size the first tab opens with (D8).
    pub term_size: TermSize,
    pub catalogue: &'static CommandCatalogue,
    pub hub: Arc<TerminalHub>,
    pub executor: Option<Arc<SharedExecutor>>,
}

/// An opened instance and the first tab to open on it.
pub struct OpenedInstance {
    pub instance: Arc<EngineInstance>,
    pub initial: InitialTabSpec,
    /// Startup messages and notices (shown to the primary as `Notice`s).
    pub notices: Vec<String>,
}

/// Builds an [`EngineInstance`] from a primary `Hello`. Production uses
/// [`StartupOpener`]; tests supply hermetic engines.
pub trait InstanceOpener: Send + Sync {
    fn open(&self, request: OpenInstanceRequest) -> Result<OpenedInstance, CommandError>;
}

/// The production opener: today's `awman` startup (`Startup`), run with the
/// instance's environment, cwd and `--set` values from its argv instead of
/// the daemon's — so config resolution (`AWMAN_HOME`/XDG/`HOME`, `AWMAN_*`,
/// `--set`) and the runtime endpoint (Docker context) are per instance.
pub struct StartupOpener {
    presentation: Arc<dyn InstancePresentation>,
}

impl StartupOpener {
    pub fn new(presentation: Arc<dyn InstancePresentation>) -> Arc<Self> {
        Arc::new(Self { presentation })
    }
}

/// The `--set` values of an argv (`--set k=v` and `--set=k=v`).
pub fn set_args_from_argv(argv: &[String]) -> Vec<String> {
    let set = crate::command::dispatch::catalogue::SET_FLAG;
    let long = format!("--{set}");
    let mut values = Vec::new();
    let mut args = argv.iter();
    while let Some(arg) = args.next() {
        if arg == &long {
            if let Some(value) = args.next() {
                values.push(value.clone());
            }
        } else if let Some(value) = arg.strip_prefix(&format!("{long}=")) {
            values.push(value.to_string());
        }
    }
    values
}

/// Whether an argv is today's bare-squad TUI launch (`awman squad`).
pub fn is_squad_launch(argv: &[String]) -> bool {
    let positionals: Vec<&String> = argv
        .iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    positionals.len() == 1 && positionals[0] == "squad"
}

fn argv_strings(env: &EnvSnapshot) -> Vec<String> {
    env.argv()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

impl InstanceOpener for StartupOpener {
    fn open(&self, request: OpenInstanceRequest) -> Result<OpenedInstance, CommandError> {
        let OpenInstanceRequest {
            kind,
            input_available,
            id,
            env,
            term_size,
            catalogue,
            hub,
            executor,
        } = request;
        let argv = argv_strings(&env);
        let overrides = ConfigOverrides::parse_set_args(&set_args_from_argv(&argv))
            .map_err(|error| CommandError::Other(format!("invalid --set: {error}")))?;
        let cwd = env.cwd().to_path_buf();
        let outcome = crate::command::startup::Startup::new(Vec::new())
            .run_with_overrides(cwd, env.clone(), overrides)
            .map_err(|error| CommandError::Other(format!("{error:#}")))?;
        let mut notices: Vec<String> = outcome.messages().to_vec();
        notices.extend(outcome.notices().iter().cloned());
        let fatal = outcome.fatal_runtime_error;
        if let Some(failure) = &fatal {
            notices.push(failure.error.to_string());
        }
        let session = outcome.session;
        let session_manager = Arc::new(SessionManager::in_memory(session.env().clone()));
        let squad_env = env.clone();
        let squad_source: EnvSource = Arc::new(move || squad_env.clone());
        let instance = EngineInstance::new_hosted(
            InstanceParts {
                catalogue,
                engines: outcome.engines,
                session_manager,
                runtime: tokio::runtime::Handle::current(),
                env: squad_source,
                presentation: Arc::clone(&self.presentation),
            },
            HostedOptions {
                id,
                kind: match kind {
                    FrontendKindWire::Tui | FrontendKindWire::Web => FrontendKind::RELAYED_TUI,
                    FrontendKindWire::Cli => FrontendKind::RELAYED_CLI,
                },
                hub: Some(hub),
                executor,
            },
        );
        instance.set_input_available(input_available);
        let initial = if fatal.is_none() && is_squad_launch(&argv) {
            InitialTabSpec::Squad { fallback: session }
        } else {
            InitialTabSpec::Session {
                session,
                fatal_startup_error: fatal.is_some() || kind == FrontendKindWire::Cli,
                pty_size: (term_size.cols, term_size.rows),
            }
        };
        Ok(OpenedInstance {
            instance,
            initial,
            notices,
        })
    }
}

// ─── Host state ─────────────────────────────────────────────────────────────

/// Everything [`EngineInstanceHost::new`] needs.
pub struct HostParts {
    pub hub: Arc<TerminalHub>,
    pub catalogue: &'static CommandCatalogue,
    pub registry: Arc<IntegrationRegistry>,
    pub opener: Arc<dyn InstanceOpener>,
    pub executor: Option<Arc<SharedExecutor>>,
    /// This awman's build string (`Welcome.build`).
    pub build: String,
}

struct FrontendRecord {
    summary: FrontendSummary,
    /// `None` while the primary is reconnecting.
    outbound: Option<tokio::sync::mpsc::Sender<EngineFrame>>,
    /// The frontend's agent-window size (D8).
    term_size: TermSize,
}

#[derive(Default)]
struct Outbox {
    presenter_events: Vec<PresenterEvent>,
}

struct InstanceState {
    lifecycle: FrontendLifecycle,
    instance: Option<Arc<EngineInstance>>,
    /// The primary's environment, cwd and argv: memory only, dropped at end.
    env: Option<EnvSnapshot>,
    annotations: Annotations,
    context_roots: Vec<PathBuf>,
    primary_kind: Option<FrontendKindWire>,
    frontends: Vec<FrontendRecord>,
    tabs: Vec<TabRelay>,
    pending: Vec<PendingDialog>,
    answered: VecDeque<(DialogId, AnsweredBy)>,
    terminal_annotations: Vec<(TerminalId, Annotations)>,
    /// Samples the instance's agent containers for `SlotStats`; built on
    /// the first pump (remote presenters have no runtime of their own).
    stats: Option<ContainerStatsSampler>,
    /// Stops the instance's squad indicator probe (TUI instances only).
    squad_indicator: Option<tokio_util::sync::CancellationToken>,
    /// The squad health last broadcast, so only changes become deltas.
    squad_health_sent: Option<SquadHealthWire>,
    /// The `agents` TUI context last handed to the tabs.
    agents_context: Option<AgentsTuiContext>,
    active_tab_hint: Option<TabId>,
    notices: Vec<String>,
}

impl InstanceState {
    fn running(&self) -> Result<Arc<EngineInstance>, HostError> {
        match (&self.instance, self.lifecycle.ended()) {
            (Some(instance), None) => Ok(Arc::clone(instance)),
            _ => Err(HostError::NotFound),
        }
    }

    fn tab(&self, tab_id: &TabId) -> Option<&TabRelay> {
        self.tabs.iter().find(|tab| tab.tab_id() == tab_id)
    }

    fn tab_mut(&mut self, tab_id: &TabId) -> Option<&mut TabRelay> {
        self.tabs.iter_mut().find(|tab| tab.tab_id() == tab_id)
    }

    fn frontend(&self, id: &FrontendId) -> Option<&FrontendRecord> {
        self.frontends.iter().find(|f| &f.summary.frontend_id == id)
    }

    fn summaries(&self) -> Vec<FrontendSummary> {
        self.frontends.iter().map(|f| f.summary.clone()).collect()
    }

    /// Send `frame` to every connected frontend except `except`. A frontend
    /// whose queue is full or closed misses it; its socket adapter closes
    /// and reports the disconnect.
    fn broadcast(&self, frame: &EngineFrame, except: Option<&FrontendId>) {
        for record in &self.frontends {
            if Some(&record.summary.frontend_id) == except {
                continue;
            }
            if let Some(outbound) = &record.outbound {
                let _ = outbound.try_send(frame.clone());
            }
        }
    }

    fn send_to(&self, id: &FrontendId, frame: EngineFrame) {
        if let Some(outbound) = self.frontend(id).and_then(|f| f.outbound.as_ref()) {
            let _ = outbound.try_send(frame);
        }
    }

    fn running_jobs(&self) -> usize {
        self.instance.as_ref().map_or(0, |i| i.running_jobs())
    }
}

struct HostedInstance {
    id: InstanceId,
    created_at: DateTime<Utc>,
    state: Mutex<InstanceState>,
}

impl HostedInstance {
    fn state(&self) -> MutexGuard<'_, InstanceState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

thread_local! {
    /// The instance a presenter callback runs for, so a presenter's
    /// `AnnotationTarget::Instance` write knows which instance it means.
    static PRESENTER_SCOPE: std::cell::RefCell<Option<InstanceId>> =
        const { std::cell::RefCell::new(None) };
}

fn with_presenter_scope<T>(instance: &InstanceId, f: impl FnOnce() -> T) -> T {
    let previous = PRESENTER_SCOPE.with(|s| s.borrow_mut().replace(instance.clone()));
    let result = f();
    PRESENTER_SCOPE.with(|s| *s.borrow_mut() = previous);
    result
}

/// The daemon's instance host.
pub struct EngineInstanceHost {
    wake: Arc<tokio::sync::Notify>,
    me: Weak<Self>,
    hub: Arc<TerminalHub>,
    catalogue: &'static CommandCatalogue,
    registry: Arc<IntegrationRegistry>,
    presenters: PresenterSupervisor,
    opener: Arc<dyn InstanceOpener>,
    executor: Option<Arc<SharedExecutor>>,
    build: String,
    instances: Mutex<Vec<Arc<HostedInstance>>>,
    draining: AtomicBool,
    share: AtomicUsize,
    settings: Mutex<Option<EngineSettings>>,
    pump_started: AtomicBool,
    launch_contexts: Mutex<super::launch_contexts::LaunchContextRegistry>,
}

/// The presenters' view of the host's annotation store (namespace already
/// checked by `PresenterHandle`).
struct HostAnnotations {
    host: Weak<EngineInstanceHost>,
}

impl AnnotationWriter for HostAnnotations {
    fn set(
        &self,
        target: &AnnotationTarget,
        namespace: &str,
        patch: &Annotations,
    ) -> Result<(), AnnotationError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| AnnotationError::NoTarget("(engine stopped)".into()))?;
        host.presenter_write(target, namespace, |current| {
            for (key, value) in patch.iter() {
                current.insert(key, value)?;
            }
            Ok(())
        })
    }

    fn clear(
        &self,
        target: &AnnotationTarget,
        namespace: &str,
        key: &str,
    ) -> Result<(), AnnotationError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| AnnotationError::NoTarget("(engine stopped)".into()))?;
        host.presenter_write(target, namespace, |current| {
            current.remove(key);
            Ok(())
        })
    }
}

/// Merge `patch` into `current`: a key with an empty value is removed, any
/// other key is set. The entry limit is checked on the result.
pub fn merge_annotations(
    current: &Annotations,
    patch: &Annotations,
) -> Result<Annotations, AnnotationError> {
    let mut merged = current.clone();
    for (key, value) in patch.iter() {
        if value.is_empty() {
            merged.remove(key);
        } else {
            merged.insert(key, value)?;
        }
    }
    Ok(merged)
}

impl EngineInstanceHost {
    pub fn new(parts: HostParts) -> Arc<Self> {
        let HostParts {
            hub,
            catalogue,
            registry,
            opener,
            executor,
            build,
        } = parts;
        Arc::new_cyclic(|me: &Weak<Self>| {
            let notices: NoticeSink = Arc::new(|level, text| {
                tracing::warn!(?level, "engine presenter: {text}");
            });
            let presenters = PresenterSupervisor::new(
                registry.presenters(),
                Arc::new(HostAnnotations { host: me.clone() }),
                notices,
            );
            Self {
                wake: Arc::new(tokio::sync::Notify::new()),
                me: me.clone(),
                hub,
                catalogue,
                registry,
                presenters,
                opener,
                executor,
                build,
                instances: Mutex::new(Vec::new()),
                draining: AtomicBool::new(false),
                share: AtomicUsize::new(0),
                settings: Mutex::new(None),
                pump_started: AtomicBool::new(false),
                launch_contexts: Mutex::new(Default::default()),
            }
        })
    }

    pub fn configure_launch_contexts(&self, path: PathBuf, env: EnvSnapshot) {
        *self.launch_contexts.lock().unwrap() =
            super::launch_contexts::LaunchContextRegistry::open(path, env);
    }
    pub fn launch_contexts(&self) -> MutexGuard<'_, super::launch_contexts::LaunchContextRegistry> {
        self.launch_contexts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn list_instances(&self) -> Vec<Arc<HostedInstance>> {
        self.instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn find(&self, id: &InstanceId) -> Option<Arc<HostedInstance>> {
        self.list_instances().into_iter().find(|i| &i.id == id)
    }

    fn forget_ended(&self) {
        self.instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|i| i.state().lifecycle.ended().is_none());
    }

    /// Start the pump task (once, inside a Tokio runtime): every
    /// [`PUMP_INTERVAL`] it relays every tab and advances lifecycle timers.
    fn ensure_pump(&self) {
        if self.pump_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.pump_started.store(false, Ordering::SeqCst);
            return;
        };
        let weak = self.me.clone();
        let wake = self.wake.clone();
        runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(PUMP_INTERVAL) => {},
                    _ = wake.notified() => {},
                }
                let Some(host) = weak.upgrade() else {
                    return;
                };
                host.pump_at(Instant::now());
            }
        });
    }

    // ── Typed operations ─────────────────────────────────────────────────

    /// `POST /v1/engine/instances`: reserve an instance for its primary
    /// `Hello` (which supplies env, cwd, argv and size). Discarded if the
    /// primary does not attach within 30 s.
    pub fn create_instance(&self, annotations: Annotations) -> Result<InstanceId, HostError> {
        if self.draining.load(Ordering::SeqCst) {
            return Err(HostError::Draining);
        }
        self.ensure_pump();
        let id = InstanceId::new();
        let hosted = Arc::new(HostedInstance {
            id: id.clone(),
            created_at: Utc::now(),
            state: Mutex::new(InstanceState {
                lifecycle: FrontendLifecycle::reserved(Instant::now()),
                instance: None,
                env: None,
                annotations,
                context_roots: vec![],
                primary_kind: None,
                frontends: Vec::new(),
                tabs: Vec::new(),
                pending: Vec::new(),
                answered: VecDeque::new(),
                terminal_annotations: Vec::new(),
                stats: None,
                squad_indicator: None,
                squad_health_sent: None,
                agents_context: None,
                active_tab_hint: None,
                notices: Vec::new(),
            }),
        });
        self.instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(hosted);
        tracing::info!(instance = %id, "engine instance reserved");
        Ok(id)
    }

    fn summary_of(hosted: &HostedInstance, state: &InstanceState) -> InstanceSummary {
        InstanceSummary {
            id: hosted.id.clone(),
            primary_frontend_kind: state.primary_kind,
            annotations: state.annotations.clone(),
            tab_count: state.tabs.len(),
            running_jobs: state.running_jobs(),
            created_at: hosted.created_at,
        }
    }

    /// `GET /v1/engine/instances?annotation=k=v`: every instance matching
    /// all filter pairs, oldest first, paged by offset.
    pub fn list(&self, filter: &AnnotationFilter, page: &Page) -> List<InstanceSummary> {
        let all: Vec<InstanceSummary> = self
            .list_instances()
            .iter()
            .filter_map(|hosted| {
                let state = hosted.state();
                (state.lifecycle.ended().is_none() && state.annotations.matches(filter))
                    .then(|| Self::summary_of(hosted, &state))
            })
            .collect();
        let start = page
            .offset_or_key
            .as_ref()
            .and_then(Cursor::offset_value)
            .unwrap_or(0)
            .min(all.len());
        let end = start.saturating_add(page.limit).min(all.len());
        let next_cursor = (end < all.len()).then(|| Cursor::offset(end).to_string());
        List {
            items: all[start..end].to_vec(),
            next_cursor,
        }
    }

    /// `GET /v1/engine/instances/{id}`.
    pub fn get(&self, id: &InstanceId) -> Option<InstanceDetail> {
        let hosted = self.find(id)?;
        let state = hosted.state();
        if state.lifecycle.ended().is_some() {
            return None;
        }
        Some(InstanceDetail {
            summary: Self::summary_of(&hosted, &state),
            frontends: state.summaries(),
            tabs: state.tabs.iter().map(TabRelay::snapshot).collect(),
        })
    }

    /// The full snapshot a frontend receives after `Welcome`.
    pub fn snapshot(&self, id: &InstanceId) -> Option<InstanceSnapshot> {
        let hosted = self.find(id)?;
        let state = hosted.state();
        Some(Self::snapshot_of(&hosted, &state))
    }

    fn snapshot_of(hosted: &HostedInstance, state: &InstanceState) -> InstanceSnapshot {
        let squad_health = state
            .instance
            .as_ref()
            .map(|i| Self::squad_health_of(i))
            .unwrap_or(SquadHealthWire::Unknown);
        InstanceSnapshot {
            instance_id: hosted.id.clone(),
            created_at: hosted.created_at,
            annotations: state.annotations.clone(),
            primary_frontend_kind: state.primary_kind,
            frontends: state.summaries(),
            tabs: state.tabs.iter().map(TabRelay::snapshot).collect(),
            active_tab_hint: state.active_tab_hint.clone(),
            squad_health,
            pending_dialogs: state.pending.clone(),
            running_jobs: state.running_jobs(),
        }
    }

    /// Give every tab of a TUI instance the `agents` context (tab numbers,
    /// containers, stuck, status, command) that a builtin TUI computes from
    /// its tabs. Only a change is written, so a watch loop sees fresh data
    /// without per-pump churn.
    fn refresh_agents_context(state: &mut InstanceState) {
        if state.primary_kind != Some(FrontendKindWire::Tui) {
            return;
        }
        let context = AgentsTuiContext::new(
            state
                .tabs
                .iter()
                .enumerate()
                .map(|(i, relay)| relay.agents_tab_snapshot((i + 1) as u32))
                .collect(),
        );
        if state.agents_context.as_ref() == Some(&context) {
            return;
        }
        for relay in &state.tabs {
            relay.set_agents_context(&context);
        }
        state.agents_context = Some(context);
    }

    /// What the instance's squad indicator probe last saw.
    fn squad_health_of(instance: &EngineInstance) -> SquadHealthWire {
        instance
            .squad_health()
            .lock()
            .map(|health| SquadHealthWire::from(*health))
            .unwrap_or(SquadHealthWire::Unknown)
    }

    /// A `SquadHealth` delta when the probe's value changed since the last
    /// broadcast. Frontends render the squad dot from it.
    fn squad_health_change(
        state: &mut InstanceState,
        instance: &EngineInstance,
    ) -> Option<InstanceEvent> {
        state.squad_indicator.as_ref()?;
        let health = Self::squad_health_of(instance);
        if state.squad_health_sent == Some(health) {
            return None;
        }
        state.squad_health_sent = Some(health);
        Some(InstanceEvent::Delta(InstanceDelta::SquadHealth(health)))
    }

    /// `PATCH /v1/engine/instances/{id}` (`{annotations}` only). Keys with an
    /// empty value are removed; the others are set.
    pub fn patch_annotations(
        &self,
        id: &InstanceId,
        patch: &Annotations,
    ) -> Result<Annotations, HostError> {
        let hosted = self.find(id).ok_or(HostError::NotFound)?;
        let mut state = hosted.state();
        if state.lifecycle.ended().is_some() {
            return Err(HostError::NotFound);
        }
        state.annotations = merge_annotations(&state.annotations, patch)?;
        let annotations = state.annotations.clone();
        state.broadcast(
            &EngineFrame::Delta(InstanceDelta::Annotations {
                target: AnnotationTarget::Instance,
                annotations: annotations.clone(),
            }),
            None,
        );
        Ok(annotations)
    }

    /// `DELETE /v1/engine/instances/{id}`: quit with today's exit policy.
    pub fn delete(&self, id: &InstanceId) -> Result<QuitPlan, HostError> {
        self.quit_instance(id, QuitReason::Api)
    }

    /// `GET /v1/engine/instances/{id}/tabs`.
    pub fn tabs(&self, id: &InstanceId) -> Option<Vec<TabSnapshot>> {
        self.get(id).map(|detail| detail.tabs)
    }

    /// `GET /v1/engine/instances/{id}/tabs/{tab}`.
    pub fn tab(&self, id: &InstanceId, tab: &TabId) -> Option<TabSnapshot> {
        let hosted = self.find(id)?;
        let state = hosted.state();
        state.tab(tab).map(TabRelay::snapshot)
    }

    /// `PATCH /v1/engine/instances/{id}/tabs/{tab}` (`{annotations}` only).
    pub fn patch_tab_annotations(
        &self,
        id: &InstanceId,
        tab: &TabId,
        patch: &Annotations,
    ) -> Result<Annotations, HostError> {
        let hosted = self.find(id).ok_or(HostError::NotFound)?;
        let mut state = hosted.state();
        let relay = state.tab_mut(tab).ok_or(HostError::UnknownTab)?;
        let merged = merge_annotations(relay.annotations(), patch)?;
        *relay.annotations_mut() = merged.clone();
        state.broadcast(
            &EngineFrame::Delta(InstanceDelta::Annotations {
                target: AnnotationTarget::Tab {
                    tab_id: tab.clone(),
                },
                annotations: merged.clone(),
            }),
            None,
        );
        Ok(merged)
    }

    /// `GET /v1/engine/instances/{id}/dialogs`: dialogs waiting for an answer.
    pub fn dialogs(&self, id: &InstanceId) -> Option<Vec<PendingDialog>> {
        let hosted = self.find(id)?;
        let state = hosted.state();
        Some(state.pending.clone())
    }

    /// Answer a pending dialog. The first answer wins: it reaches the
    /// command thread, and every other frontend gets `DialogClosed{by}`
    /// (`POST /v1/engine/instances/{id}/dialogs/{id}/answer` uses
    /// `AnsweredBy::Api`; a frontend's `DialogResponse` its own id).
    pub fn answer(
        &self,
        id: &InstanceId,
        dialog_id: &DialogId,
        response: DialogResponse,
        by: AnsweredBy,
    ) -> Result<DialogAnswerOutcome, HostError> {
        let hosted = self.find(id).ok_or(HostError::NotFound)?;
        let mut state = hosted.state();
        let Some(idx) = state.pending.iter().position(|p| &p.dialog_id == dialog_id) else {
            return match state.answered.iter().find(|(d, _)| d == dialog_id) {
                Some((_, by)) => Ok(DialogAnswerOutcome::AlreadyAnswered { by: by.clone() }),
                None => Err(HostError::UnknownDialog),
            };
        };
        if !state.pending[idx].request.accepts(&response) {
            return Ok(DialogAnswerOutcome::Rejected);
        }
        let pending = state.pending.remove(idx);
        let delivered = state
            .tab(&pending.tab_id)
            .map(|relay| relay.answer(response))
            .unwrap_or_else(|| Err(CommandError::Other("the dialog's tab is gone".into())));
        Self::remember_answer(&mut state, dialog_id.clone(), by.clone());
        let except = match &by {
            AnsweredBy::Frontend { frontend_id } => Some(frontend_id.clone()),
            _ => None,
        };
        state.broadcast(
            &EngineFrame::DialogClosed {
                dialog_id: dialog_id.clone(),
                answered_by: by,
            },
            except.as_ref(),
        );
        state.broadcast(
            &EngineFrame::Delta(InstanceDelta::DialogResolved {
                dialog_id: dialog_id.clone(),
            }),
            None,
        );
        delivered?;
        Ok(DialogAnswerOutcome::Answered)
    }

    fn remember_answer(state: &mut InstanceState, dialog_id: DialogId, by: AnsweredBy) {
        state.answered.push_back((dialog_id, by));
        while state.answered.len() > ANSWERED_MEMORY {
            state.answered.pop_front();
        }
    }

    /// `POST /v1/engine/instances/{id}/ui_requests` (`from: None`) or a
    /// frontend's `UiRequest` frame. The request goes to the host presenter
    /// that claims the frontend (the primary, for API requests); a presenter
    /// that declines, panics or times out leaves it unclaimed and the
    /// instance unaffected. Unclaimed requests are the frontend's to handle
    /// natively, exactly like today's TUI. An unclaimed API request for a
    /// tab mutation (`open_tab`, `close_tab`) is applied by the engine.
    pub async fn ui_request(
        &self,
        id: &InstanceId,
        from: Option<FrontendId>,
        request: UiRequest,
    ) -> Result<UiOutcome, HostError> {
        let hosted = self.find(id).ok_or(HostError::NotFound)?;
        let (presenter, frontend) = {
            let state = hosted.state();
            state.running()?;
            let frontend = match &from {
                Some(fid) => state.frontend(fid).map(|f| f.summary.clone()),
                None => state
                    .lifecycle
                    .primary()
                    .and_then(|p| state.frontend(p))
                    .map(|f| f.summary.clone()),
            };
            let presenter = frontend
                .as_ref()
                .and_then(|f| self.registry.presenter_for(&f.annotations))
                .or_else(|| self.registry.presenter_for(&state.annotations));
            (presenter, frontend)
        };
        let outcome = match presenter {
            Some(presenter) => {
                let frontend_id = frontend
                    .as_ref()
                    .map(|f| f.frontend_id.clone())
                    .unwrap_or_else(FrontendId::new);
                self.call_presenter(&hosted, presenter, frontend_id, request.clone())
                    .await
            }
            None => UiOutcome::unclaimed(),
        };
        if !outcome.claimed_by_host && from.is_none() {
            return self.apply_api_ui_request(id, request, outcome);
        }
        Ok(outcome)
    }

    async fn call_presenter(
        &self,
        hosted: &Arc<HostedInstance>,
        presenter: Arc<dyn HostPresenter>,
        frontend: FrontendId,
        request: UiRequest,
    ) -> UiOutcome {
        let notices: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_notices = Arc::clone(&notices);
        let sink: NoticeSink = Arc::new(move |_level, text| {
            if let Ok(mut notices) = sink_notices.lock() {
                notices.push(text);
            }
        });
        let instance = hosted.id.clone();
        let name = presenter.name();
        let job = tokio::task::spawn_blocking(move || {
            with_presenter_scope(&instance, || {
                call_ui_request(presenter.as_ref(), &instance, &frontend, &request, &sink)
            })
        });
        let outcome = match tokio::time::timeout(PRESENTER_TIMEOUT, job).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) | Err(_) => {
                if let Ok(mut notices) = notices.lock() {
                    notices.push(format!(
                        "integration `{name}` could not handle a UI request; handled by the frontend instead"
                    ));
                }
                UiOutcome::unclaimed()
            }
        };
        let notices: Vec<String> = notices.lock().map(|n| n.clone()).unwrap_or_default();
        if !notices.is_empty() {
            let state = hosted.state();
            for text in notices {
                state.broadcast(
                    &EngineFrame::Notice {
                        level: MessageLevel::Warning,
                        text,
                    },
                    None,
                );
            }
        }
        outcome
    }

    fn apply_api_ui_request(
        &self,
        id: &InstanceId,
        request: UiRequest,
        outcome: UiOutcome,
    ) -> Result<UiOutcome, HostError> {
        match request {
            UiRequest::OpenTab { dir } => {
                self.open_tab(id, None, dir, OpenTabOptions::default())?;
                Ok(UiOutcome {
                    detail: Some("opened by the engine".into()),
                    ..outcome
                })
            }
            UiRequest::CloseTab { tab_id } => {
                self.close_tab(id, &tab_id)?;
                Ok(UiOutcome {
                    detail: Some("closed by the engine".into()),
                    ..outcome
                })
            }
            _ => Ok(outcome),
        }
    }

    // ── Frontends ────────────────────────────────────────────────────────

    /// Admit a frontend whose first frame was `hello`. Frames for it go to
    /// `outbound` (Layer 3 encodes and sends them); its inbound frames go to
    /// the returned session. A primary's first `Hello` opens the instance
    /// with its environment, cwd, argv and size.
    pub async fn attach_frontend(
        &self,
        id: InstanceId,
        hello: Hello,
        outbound: tokio::sync::mpsc::Sender<EngineFrame>,
    ) -> Result<FrontendSession, AttachRefusal> {
        let hosted = self.find(&id).ok_or(AttachRefusal::NotFound)?;
        let admitted = {
            let mut state = hosted.state();
            let draining = self.draining.load(Ordering::SeqCst);
            let tab_ids: Vec<TabId> = state.tabs.iter().map(|t| t.tab_id().clone()).collect();
            if (hello.resume.is_some() && state.primary_kind != Some(hello.kind))
                || (hello.kind == FrontendKindWire::Web
                    && hello.role == FrontendRole::Secondary
                    && state.primary_kind != Some(FrontendKindWire::Web))
            {
                return Err(AttachRefusal::ProtocolError(
                    "frontend kind does not match the instance".into(),
                ));
            }
            admit(
                &hello,
                draining,
                &mut state.lifecycle,
                |tab| tab_ids.contains(tab),
                Instant::now(),
            )?
        };
        if let Admitted::StartingPrimary { .. } = admitted {
            match self.start_instance(&hosted, &hello).await {
                Ok(outbox) => self.dispatch_presenter_events(&id, outbox),
                Err(error) => {
                    let mut state = hosted.state();
                    self.end_locked(
                        &hosted,
                        &mut state,
                        QuitReason::NeverAttached,
                        &mut Outbox::default(),
                    );
                    drop(state);
                    self.forget_ended();
                    return Err(AttachRefusal::StartFailed(error.to_string()));
                }
            }
        }
        let frontend_id = admitted.frontend_id().clone();
        let role = admitted.role();
        {
            let mut state = hosted.state();
            let summary = FrontendSummary {
                frontend_id: frontend_id.clone(),
                kind: hello.kind,
                role,
                annotations: hello.annotations.clone(),
                connected_at: Utc::now(),
                view: hello.view.clone(),
                reconnecting: false,
            };
            match state
                .frontends
                .iter_mut()
                .find(|f| f.summary.frontend_id == frontend_id)
            {
                // The primary back within its grace keeps its id.
                Some(record) => {
                    record.summary.reconnecting = false;
                    record.outbound = Some(outbound.clone());
                    record.term_size = hello.term_size;
                }
                None => {
                    state.broadcast(
                        &EngineFrame::Delta(InstanceDelta::FrontendAttached(summary.clone())),
                        None,
                    );
                    state.frontends.push(FrontendRecord {
                        summary,
                        outbound: Some(outbound.clone()),
                        term_size: hello.term_size,
                    });
                }
            }
            if role == FrontendRole::Primary {
                state.primary_kind = Some(hello.kind);
            }
            state.send_to(
                &frontend_id,
                EngineFrame::Welcome {
                    frontend_id: frontend_id.clone(),
                    instance_id: id.clone(),
                    protocol: FRONTEND_SUBPROTOCOL.to_string(),
                    build: self.build.clone(),
                    role,
                },
            );
            let snapshot = Self::snapshot_of(&hosted, &state);
            match chunk_snapshot(&snapshot) {
                Ok(frames) => {
                    for frame in frames {
                        state.send_to(&frontend_id, frame);
                    }
                }
                Err(error) => state.send_to(
                    &frontend_id,
                    EngineFrame::Notice {
                        level: MessageLevel::Error,
                        text: format!("instance snapshot unavailable: {error}"),
                    },
                ),
            }
            if role == FrontendRole::Primary {
                for text in std::mem::take(&mut state.notices) {
                    state.send_to(
                        &frontend_id,
                        EngineFrame::Notice {
                            level: MessageLevel::Info,
                            text,
                        },
                    );
                }
            }
        }
        tracing::info!(
            instance = %id,
            frontend = %frontend_id,
            role = ?role,
            kind = ?hello.kind,
            "engine frontend attached"
        );
        Ok(FrontendSession {
            host: self.me.clone(),
            frontend_id,
            instance_id: id,
            role,
            done: false,
        })
    }

    /// Open the instance for its primary and its first tab, exactly as
    /// `tui::run()` does.
    async fn start_instance(
        &self,
        hosted: &Arc<HostedInstance>,
        hello: &Hello,
    ) -> Result<Outbox, CommandError> {
        let (env, stale, roots) = if hello.kind == FrontendKindWire::Web {
            let contexts = self.launch_contexts();
            let id = hello
                .launch_context
                .as_ref()
                .ok_or_else(|| CommandError::Other("missing launch_context".into()))?;
            let (env, stale) = contexts
                .resolve_env(id)
                .map_err(|e| CommandError::Other(e.to_string()))?;
            (env, stale, contexts.roots())
        } else {
            (
                EnvSnapshot::from_instance_env(
                    hello.env.clone().into_inner().into_iter().collect(),
                    hello.cwd.clone(),
                    hello.argv.clone(),
                ),
                false,
                vec![],
            )
        };
        let request = OpenInstanceRequest {
            kind: hello.kind,
            input_available: hello.kind == FrontendKindWire::Web || hello.stdin_is_tty,
            id: hosted.id.clone(),
            env: env.clone(),
            term_size: hello.term_size,
            catalogue: self.catalogue,
            hub: Arc::clone(&self.hub),
            executor: self.executor.clone(),
        };
        let opener = Arc::clone(&self.opener);
        let wake = self.wake.clone();
        // Startup and the squad open block (runtime detection, the squad
        // daemon hand-shake): never on a runtime worker.
        let opened = tokio::task::spawn_blocking(move || {
            let opened = opener.open(request)?;
            opened.instance.set_wake(wake);
            let initial = opened.instance.open_initial(opened.initial);
            Ok::<_, CommandError>((opened.instance, initial, opened.notices))
        })
        .await
        .map_err(|error| CommandError::Other(format!("instance start failed: {error}")))??;
        let (instance, initial, notices) = opened;
        let size = (hello.term_size.cols, hello.term_size.rows);
        let mut state = hosted.state();
        state.env = Some(env);
        state.context_roots = roots;
        state.notices = notices;
        if stale {
            state.notices.push(format!("Environment stale; run awman daemon api launch_contexts.refresh id={} from the context root to refresh shell PATH and credentials.", hello.launch_context.as_ref().expect("web context resolved")));
        }
        let mut events = Vec::new();
        match initial {
            InitialTabOpened::Normal { tab } => {
                events.extend(self.add_tab(&mut state, &instance, tab, TabKindWire::Normal, size));
            }
            InitialTabOpened::SquadFailed { tab, error } => {
                state.notices.push(error.to_string());
                events.extend(self.add_tab(&mut state, &instance, tab, TabKindWire::Normal, size));
            }
            InitialTabOpened::Squad { session, .. } => {
                if let Some(tab_id) = instance.tab_id_for(&session.id()) {
                    state.tabs.push(TabRelay::new(
                        tab_id,
                        &session,
                        TabKindWire::Squad,
                        Default::default(),
                        None,
                        size,
                    ));
                }
            }
        }
        // A TUI shows the squad indicator; the engine probes for it, as the
        // builtin backend's TUI does, and relays changes as deltas.
        if hello.kind == FrontendKindWire::Tui {
            let cancel = tokio_util::sync::CancellationToken::new();
            instance.start_squad_indicator(cancel.clone());
            state.squad_indicator = Some(cancel);
        }
        state.instance = Some(instance);
        let mut outbox = Outbox::default();
        self.apply_events(hosted, &mut state, events, &mut outbox);
        Ok(outbox)
    }

    /// Register a tab the instance opened and bind its startup command.
    fn add_tab(
        &self,
        state: &mut InstanceState,
        instance: &Arc<EngineInstance>,
        tab: OpenedTab,
        kind: TabKindWire,
        size: (u16, u16),
    ) -> Vec<InstanceEvent> {
        let OpenedTab {
            session,
            feeds,
            startup,
            ..
        } = tab;
        let Some(tab_id) = instance.tab_id_for(&session.id()) else {
            return Vec::new();
        };
        let mut relay = TabRelay::new(
            tab_id.clone(),
            &session,
            kind,
            feeds,
            Some(Arc::clone(&self.hub)),
            size,
        );
        let mut events = vec![InstanceEvent::TabOpened { tab_id }];
        match startup {
            StartupRun::NotStarted => {}
            StartupRun::Started(submitted) => events.extend(relay.bind(submitted)),
            StartupRun::Refused(error) => events.push(InstanceEvent::Notice {
                level: MessageLevel::Error,
                text: error.to_string(),
            }),
        }
        state.tabs.push(relay);
        events
    }

    fn frontend_disconnected(&self, id: &InstanceId, frontend: &FrontendId) {
        let Some(hosted) = self.find(id) else {
            return;
        };
        let mut state = hosted.state();
        match state.lifecycle.disconnected(frontend, Instant::now()) {
            LifecycleAction::Grace { .. } => {
                if let Some(record) = state
                    .frontends
                    .iter_mut()
                    .find(|f| &f.summary.frontend_id == frontend)
                {
                    record.summary.reconnecting = true;
                    record.outbound = None;
                }
                tracing::info!(instance = %id, frontend = %frontend, "engine primary frontend lost; waiting for a reconnect");
            }
            _ => {
                let before = state.frontends.len();
                state
                    .frontends
                    .retain(|f| &f.summary.frontend_id != frontend);
                if state.frontends.len() != before {
                    tracing::info!(instance = %id, frontend = %frontend, "engine frontend detached");
                    state.broadcast(
                        &EngineFrame::Delta(InstanceDelta::FrontendDetached {
                            frontend_id: frontend.clone(),
                            reason: "hangup".into(),
                        }),
                        None,
                    );
                }
            }
        }
    }

    // ── Tabs and commands ────────────────────────────────────────────────

    fn frontend_size(state: &InstanceState, from: Option<&FrontendId>) -> (u16, u16) {
        let record = match from {
            Some(id) => state.frontend(id),
            None => state.lifecycle.primary().and_then(|p| state.frontend(p)),
        };
        record
            .map(|r| (r.term_size.cols, r.term_size.rows))
            .unwrap_or(super::feeds::SharedPtySize::DEFAULT)
    }

    /// Open a directory tab (Ctrl-T): the instance registers it and starts
    /// its startup command.
    pub fn open_tab(
        &self,
        id: &InstanceId,
        from: Option<&FrontendId>,
        dir: std::path::PathBuf,
        options: OpenTabOptions,
    ) -> Result<TabId, HostError> {
        if self.draining.load(Ordering::SeqCst) {
            return Err(HostError::Draining);
        }
        let hosted = self.find(id).ok_or(HostError::NotFound)?;
        let (instance, size, web_context) = {
            let state = hosted.state();
            let web_context = if state.primary_kind == Some(FrontendKindWire::Web)
                && options.kind != TabKindWire::Squad
            {
                Some((
                    state.context_roots.clone(),
                    state
                        .env
                        .as_ref()
                        .ok_or(HostError::NotFound)?
                        .cwd()
                        .to_path_buf(),
                ))
            } else {
                None
            };
            (
                state.running()?,
                Self::frontend_size(&state, from),
                web_context,
            )
        };
        let dir = if let Some((roots, cwd)) = web_context {
            if let Some(lease_id) = options.lease_id.as_deref() {
                // The instance's own worker prepared this path for this exact
                // lease. A browser cannot use the lease field to name any
                // other directory outside its launch context.
                let prepared = instance
                    .prepared_lease_run(lease_id)
                    .is_some_and(|run| run.cwd == dir);
                if !prepared
                    || options.startup != crate::data::frontend_protocol::StartupSpawn::None
                {
                    return Err(HostError::DirOutsideContext);
                }
                dir
            } else {
                super::launch_contexts::LaunchContextRegistry::resolve_dir(&roots, &cwd, &dir)
                    .map_err(|_| HostError::DirOutsideContext)?
            }
        } else {
            dir
        };
        let kind = options.kind.clone();
        let opened = if kind == TabKindWire::Squad {
            let session = instance
                .open_squad_session()
                .map_err(|e| CommandError::Other(e.to_string()))?;
            instance.opened(session, false, size)
        } else {
            instance.open_tab_with_startup(
                OpenTabRequest {
                    dir,
                    options: SessionOpenOptions::default(),
                    pty_size: size,
                },
                options.startup != crate::data::frontend_protocol::StartupSpawn::None,
            )?
        };
        let session_id = opened.session.id();
        let mut state = hosted.state();
        let events = self.add_tab(&mut state, &instance, opened, kind, size);
        let tab_id = instance
            .tab_id_for(&session_id)
            .ok_or(HostError::UnknownTab)?;
        if let Some(relay) = state.tab_mut(&tab_id) {
            *relay.annotations_mut() = options.annotations;
        }
        let mut outbox = Outbox::default();
        self.apply_events(&hosted, &mut state, events, &mut outbox);
        drop(state);
        self.dispatch_presenter_events(id, outbox);
        Ok(tab_id)
    }

    /// Close a tab with today's semantics (cancel its command, end it).
    /// Closing the last tab quits the instance.
    pub fn close_tab(&self, id: &InstanceId, tab_id: &TabId) -> Result<(), HostError> {
        let hosted = self.find(id).ok_or(HostError::NotFound)?;
        let mut outbox = Outbox::default();
        let mut state = hosted.state();
        let instance = state.running()?;
        let idx = state
            .tabs
            .iter()
            .position(|t| t.tab_id() == tab_id)
            .ok_or(HostError::UnknownTab)?;
        let mut relay = state.tabs.remove(idx);
        instance.close_tab(&relay.session_id());
        let mut events = relay.close();
        events.push(InstanceEvent::TabClosed {
            tab_id: tab_id.clone(),
        });
        self.resolve_tab_dialogs(&mut state, tab_id);
        drop(relay);
        self.apply_events(&hosted, &mut state, events, &mut outbox);
        let remaining = state.tabs.len();
        if let LifecycleAction::Quit(reason) = state.lifecycle.tab_closed(remaining) {
            self.end_locked(&hosted, &mut state, reason, &mut outbox);
        }
        drop(state);
        self.dispatch_presenter_events(id, outbox);
        self.forget_ended();
        Ok(())
    }

    /// Pending dialogs of a tab whose command is gone are closed by the engine.
    fn resolve_tab_dialogs(&self, state: &mut InstanceState, tab_id: &TabId) {
        let (gone, kept): (Vec<PendingDialog>, Vec<PendingDialog>) =
            std::mem::take(&mut state.pending)
                .into_iter()
                .partition(|p| &p.tab_id == tab_id);
        state.pending = kept;
        for dialog in gone {
            Self::remember_answer(state, dialog.dialog_id.clone(), AnsweredBy::Engine);
            state.broadcast(
                &EngineFrame::DialogClosed {
                    dialog_id: dialog.dialog_id.clone(),
                    answered_by: AnsweredBy::Engine,
                },
                None,
            );
            state.broadcast(
                &EngineFrame::Delta(InstanceDelta::DialogResolved {
                    dialog_id: dialog.dialog_id,
                }),
                None,
            );
        }
    }

    /// Submit a command in a tab: parse it in Layer 2 (D3), admit it, reset
    /// the tab's feeds and start it through the instance's Dispatch.
    pub fn submit(
        &self,
        id: &InstanceId,
        from: Option<&FrontendId>,
        tab_id: &TabId,
        input: SubmitInput,
        pty_size: TermSize,
    ) -> Result<(), HostError> {
        if self.draining.load(Ordering::SeqCst) {
            return Err(HostError::Draining);
        }
        let hosted = self.find(id).ok_or(HostError::NotFound)?;
        let parsed = parse_submit(self.catalogue, &input)?;
        let command_path = parsed.path.clone();
        let mut state = hosted.state();
        let instance = state.running()?;
        let relay = state.tab_mut(tab_id).ok_or(HostError::UnknownTab)?;
        // Refuse before the tab is reset, as the TUI does.
        instance.admit(&parsed.path)?;
        let size = (pty_size.cols, pty_size.rows);
        relay.set_pty_size(size);
        let (feeds, mut events) = relay.reset_for_submit();
        let submitted = instance.submit(SubmitRequest {
            lease_id: match &input {
                SubmitInput::Lease { lease_id, .. } => Some(lease_id.clone()),
                _ => None,
            },
            session_id: relay.session_id(),
            parsed,
            feeds,
            pty_size: size,
            squad_attach: None,
        })?;
        events.extend(relay.bind(submitted));
        tracing::info!(
            instance = %id,
            tab = %tab_id,
            command = %command_path.join(" "),
            lease = matches!(input, SubmitInput::Lease { .. }),
            "engine command started"
        );
        let mut outbox = Outbox::default();
        self.apply_events(&hosted, &mut state, events, &mut outbox);
        let _ = from;
        drop(state);
        self.dispatch_presenter_events(id, outbox);
        Ok(())
    }

    fn with_relay(
        &self,
        id: &InstanceId,
        tab_id: &TabId,
        f: impl FnOnce(&mut TabRelay),
    ) -> Result<(), HostError> {
        let hosted = self.find(id).ok_or(HostError::NotFound)?;
        let mut state = hosted.state();
        let relay = state.tab_mut(tab_id).ok_or(HostError::UnknownTab)?;
        f(relay);
        Ok(())
    }

    /// Quit an instance with today's exit policy.
    pub fn quit_instance(
        &self,
        id: &InstanceId,
        reason: QuitReason,
    ) -> Result<QuitPlan, HostError> {
        let hosted = self.find(id).ok_or(HostError::NotFound)?;
        let mut outbox = Outbox::default();
        let plan = {
            let mut state = hosted.state();
            if state.lifecycle.ended().is_some() {
                return Err(HostError::NotFound);
            }
            state.lifecycle.quit(reason);
            self.end_locked(&hosted, &mut state, reason, &mut outbox)
        };
        self.dispatch_presenter_events(id, outbox);
        self.forget_ended();
        Ok(plan)
    }

    /// End an instance (its lifecycle already ended): today's exit policy,
    /// `Detach` for every frontend, and the environment dropped.
    fn end_locked(
        &self,
        hosted: &HostedInstance,
        state: &mut InstanceState,
        reason: QuitReason,
        outbox: &mut Outbox,
    ) -> QuitPlan {
        if state.lifecycle.ended().is_none() {
            state.lifecycle.quit(reason);
        }
        let plan = match state.instance.take() {
            Some(instance) => instance.quit(),
            None => QuitPlan {
                detach_containers: false,
                exit_code: 0,
                cancel_running: true,
            },
        };
        let mut events = Vec::new();
        for mut relay in std::mem::take(&mut state.tabs) {
            events.extend(relay.close());
        }
        for event in events {
            if let Some(event) = presenter_event(&event) {
                outbox.presenter_events.push(event);
            }
        }
        state.broadcast(&EngineFrame::Detach(reason.detach_reason()), None);
        state.frontends.clear();
        state.pending.clear();
        state.terminal_annotations.clear();
        // Memory only: the instance's environment ends with it.
        state.env = None;
        if let Some(cancel) = state.squad_indicator.take() {
            cancel.cancel();
        }
        outbox.presenter_events.push(PresenterEvent::Ended);
        tracing::info!(instance = %hosted.id, reason = ?reason, "engine instance ended");
        plan
    }

    // ── Pumping ──────────────────────────────────────────────────────────

    /// Relay every tab once and advance lifecycle timers to `now`.
    pub fn pump_at(&self, now: Instant) {
        for hosted in self.list_instances() {
            let mut outbox = Outbox::default();
            {
                let mut state = hosted.state();
                if let LifecycleAction::Quit(reason) = state.lifecycle.poll(now) {
                    self.end_locked(&hosted, &mut state, reason, &mut outbox);
                } else if let Some(instance) = state.instance.clone() {
                    let mut events = Vec::new();
                    for relay in &mut state.tabs {
                        let session = instance.session_snapshot(&relay.session_id());
                        events.extend(relay.pump(session.as_ref()));
                    }
                    events.extend(Self::sample_stats(&mut state, &instance));
                    events.extend(Self::squad_health_change(&mut state, &instance));
                    Self::refresh_agents_context(&mut state);
                    self.apply_events(&hosted, &mut state, events, &mut outbox);
                }
            }
            self.dispatch_presenter_events(&hosted.id, outbox);
        }
        self.forget_ended();
    }

    /// Route arrived container stats to their slots, and start a new round
    /// when one is due. The sampler owns the cadence and runs each query on
    /// the blocking pool, so a pump never waits on the runtime CLI.
    fn sample_stats(state: &mut InstanceState, instance: &EngineInstance) -> Vec<InstanceEvent> {
        let InstanceState { tabs, stats, .. } = state;
        let sampler = stats.get_or_insert_with(|| instance.stats_sampler());
        let mut events = Vec::new();
        for sample in sampler.drain() {
            // The key is a tab index; the slot id guards against a shift.
            if let Some(relay) = tabs.get_mut(sample.key) {
                events.extend(relay.apply_stats(&sample.step_name, sample.stats));
            }
        }
        if sampler.due() {
            let requests: Vec<StatsRequest> = tabs
                .iter()
                .enumerate()
                .flat_map(|(key, relay)| {
                    relay
                        .stats_targets()
                        .into_iter()
                        .map(move |(slot, container)| StatsRequest {
                            key,
                            step_name: slot,
                            target: StatsTarget::Named(container),
                        })
                })
                .collect();
            sampler.poll(requests);
        }
        events
    }

    /// Turn relay events into frames (assigning dialog ids) and presenter
    /// events.
    fn apply_events(
        &self,
        hosted: &HostedInstance,
        state: &mut InstanceState,
        events: Vec<InstanceEvent>,
        outbox: &mut Outbox,
    ) {
        for event in events {
            if let Some(presenter) = presenter_event(&event) {
                outbox.presenter_events.push(presenter);
            }
            log_instance_event(&hosted.id, &event);
            match event {
                InstanceEvent::CommandBytes {
                    tab_id,
                    stream,
                    bytes,
                } => state.broadcast(
                    &EngineFrame::CommandBytes {
                        tab_id,
                        stream,
                        bytes,
                    },
                    None,
                ),
                InstanceEvent::Delta(delta) => state.broadcast(&EngineFrame::Delta(delta), None),
                InstanceEvent::CommandOutput {
                    tab_id,
                    stream,
                    level,
                    text,
                } => state.broadcast(
                    &EngineFrame::CommandOutput {
                        tab_id,
                        stream: if state
                            .instance
                            .as_ref()
                            .is_some_and(|i| i.kind() == FrontendKind::RELAYED_CLI)
                        {
                            crate::data::frontend_protocol::OutputStream::Stderr
                        } else {
                            stream
                        },
                        level,
                        text,
                    },
                    None,
                ),
                InstanceEvent::CommandFinished { tab_id, result } => {
                    // A finished command answers nothing more.
                    self.resolve_tab_dialogs(state, &tab_id);
                    state.broadcast(&EngineFrame::CommandFinished { tab_id, result }, None);
                }
                InstanceEvent::DialogRaised { tab_id, request } => {
                    let dialog_id = DialogId::new();
                    if request.awaits_answer() {
                        let pending = PendingDialog {
                            dialog_id: dialog_id.clone(),
                            tab_id: tab_id.clone(),
                            request: request.clone(),
                            raised_at: Utc::now(),
                        };
                        state.pending.push(pending.clone());
                        state.broadcast(
                            &EngineFrame::Delta(InstanceDelta::DialogPending(pending)),
                            None,
                        );
                    }
                    state.broadcast(
                        &EngineFrame::DialogRequest {
                            dialog_id,
                            tab_id,
                            request,
                        },
                        None,
                    );
                }
                InstanceEvent::DialogClosed {
                    dialog_id,
                    answered_by,
                } => state.broadcast(
                    &EngineFrame::DialogClosed {
                        dialog_id,
                        answered_by,
                    },
                    None,
                ),
                InstanceEvent::ProgramStatus {
                    tab_id,
                    slot_id,
                    status,
                } => state.broadcast(
                    &EngineFrame::ProgramStatus {
                        tab_id,
                        slot_id,
                        status,
                    },
                    None,
                ),
                InstanceEvent::TabOpened { tab_id } => {
                    if let Some(snapshot) = state.tab(&tab_id).map(TabRelay::snapshot) {
                        state.broadcast(
                            &EngineFrame::Delta(InstanceDelta::TabOpened(snapshot)),
                            None,
                        );
                    }
                }
                InstanceEvent::TabClosed { tab_id } => state.broadcast(
                    &EngineFrame::Delta(InstanceDelta::TabClosed { tab_id }),
                    None,
                ),
                InstanceEvent::Notice { level, text } => {
                    state.broadcast(&EngineFrame::Notice { level, text }, None)
                }
                InstanceEvent::ContainerWindow { .. }
                | InstanceEvent::TerminalOpened { .. }
                | InstanceEvent::TerminalClosed { .. }
                | InstanceEvent::TerminalViewerDetached { .. }
                | InstanceEvent::Ended(_) => {}
            }
        }
        let _ = hosted;
    }

    fn dispatch_presenter_events(&self, instance: &InstanceId, outbox: Outbox) {
        for event in outbox.presenter_events {
            with_presenter_scope(instance, || {
                self.presenters.instance_event(instance, &event)
            });
        }
    }

    // ── Annotations ──────────────────────────────────────────────────────

    /// A presenter's write (namespace already checked by its handle).
    fn presenter_write(
        &self,
        target: &AnnotationTarget,
        _namespace: &str,
        write: impl FnOnce(&mut Annotations) -> Result<(), AnnotationError>,
    ) -> Result<(), AnnotationError> {
        let scope = PRESENTER_SCOPE.with(|s| s.borrow().clone());
        let hosted = match target {
            AnnotationTarget::Instance => scope.and_then(|id| self.find(&id)),
            AnnotationTarget::Tab { tab_id } => {
                self.instance_of(|state| state.tab(tab_id).is_some())
            }
            AnnotationTarget::Frontend { frontend_id } => {
                self.instance_of(|state| state.frontend(frontend_id).is_some())
            }
            AnnotationTarget::Terminal { terminal_id } => self.instance_of(|state| {
                state.tabs.iter().any(|t| t.owns_terminal(terminal_id))
                    || state
                        .terminal_annotations
                        .iter()
                        .any(|(t, _)| t == terminal_id)
            }),
        }
        .ok_or_else(|| AnnotationError::NoTarget(format!("{target:?}")))?;
        let mut state = hosted.state();
        let annotations = match target {
            AnnotationTarget::Instance => {
                let mut next = state.annotations.clone();
                write(&mut next)?;
                state.annotations = next.clone();
                next
            }
            AnnotationTarget::Tab { tab_id } => {
                let relay = state
                    .tab_mut(tab_id)
                    .ok_or_else(|| AnnotationError::NoTarget(format!("{target:?}")))?;
                let mut next = relay.annotations().clone();
                write(&mut next)?;
                *relay.annotations_mut() = next.clone();
                next
            }
            AnnotationTarget::Frontend { frontend_id } => {
                let record = state
                    .frontends
                    .iter_mut()
                    .find(|f| &f.summary.frontend_id == frontend_id)
                    .ok_or_else(|| AnnotationError::NoTarget(format!("{target:?}")))?;
                let mut next = record.summary.annotations.clone();
                write(&mut next)?;
                record.summary.annotations = next.clone();
                next
            }
            AnnotationTarget::Terminal { terminal_id } => {
                let idx = match state
                    .terminal_annotations
                    .iter()
                    .position(|(t, _)| t == terminal_id)
                {
                    Some(idx) => idx,
                    None => {
                        state
                            .terminal_annotations
                            .push((terminal_id.clone(), Annotations::new()));
                        state.terminal_annotations.len() - 1
                    }
                };
                let mut next = state.terminal_annotations[idx].1.clone();
                write(&mut next)?;
                state.terminal_annotations[idx].1 = next.clone();
                next
            }
        };
        state.broadcast(
            &EngineFrame::Delta(InstanceDelta::Annotations {
                target: target.clone(),
                annotations,
            }),
            None,
        );
        Ok(())
    }

    fn instance_of(&self, test: impl Fn(&InstanceState) -> bool) -> Option<Arc<HostedInstance>> {
        self.list_instances()
            .into_iter()
            .find(|hosted| test(&hosted.state()))
    }

    /// The annotations recorded on a terminal (presenter-written; terminal
    /// subscription annotations live with the subscription, 0141).
    pub fn terminal_annotations(&self, terminal: &TerminalId) -> Option<Annotations> {
        self.list_instances().into_iter().find_map(|hosted| {
            let state = hosted.state();
            state
                .terminal_annotations
                .iter()
                .find(|(t, _)| t == terminal)
                .map(|(_, a)| a.clone())
        })
    }

    // ── Terminals ────────────────────────────────────────────────────────

    /// The instance a `tab_` terminal owner belongs to.
    pub fn instance_for_tab(&self, tab: &TabId) -> Option<InstanceId> {
        self.instance_of(|state| state.tab(tab).is_some())
            .map(|hosted| hosted.id.clone())
    }

    /// The tabs of an instance (`GET /v1/terminals?owner_id=ins_…` lists
    /// their terminals).
    pub fn instance_tab_ids(&self, id: &InstanceId) -> Option<Vec<TabId>> {
        let hosted = self.find(id)?;
        let state = hosted.state();
        Some(state.tabs.iter().map(|t| t.tab_id().clone()).collect())
    }

    /// A terminal event for a `tab_` owner (the daemon's terminal event log
    /// routes them here): viewer detaches reach the host presenters.
    pub fn terminal_event(
        &self,
        tab: &TabId,
        payload: &crate::data::execution_event::EventPayload,
    ) {
        use crate::data::execution_event::EventPayload;
        let Some(id) = self.instance_for_tab(tab) else {
            return;
        };
        let event = match payload {
            EventPayload::TerminalViewerDetached {
                terminal_id,
                reason,
                annotations,
            } => PresenterEvent::TerminalViewerDetached {
                terminal_id: terminal_id.clone(),
                reason: reason.as_str().to_string(),
                annotations: annotations.clone(),
            },
            _ => return,
        };
        self.dispatch_presenter_events(
            &id,
            Outbox {
                presenter_events: vec![event],
            },
        );
    }

    // ── Counters ─────────────────────────────────────────────────────────

    pub fn instance_count(&self) -> usize {
        self.list_instances().len()
    }

    pub fn running_jobs(&self) -> usize {
        self.list_instances()
            .iter()
            .map(|hosted| hosted.state().running_jobs())
            .sum()
    }

    /// The executor share last applied (`daemon.features.engine.maxConcurrentJobs`).
    pub fn executor_share(&self) -> usize {
        self.share.load(Ordering::SeqCst)
    }

    async fn end_all(&self, reason: QuitReason) {
        for hosted in self.list_instances() {
            let _ = self.quit_instance(&hosted.id, reason);
        }
    }
}

/// Parse a `Submit` in Layer 2, at the same point the builtin path does (D3).
/// The daemon log's view of an instance event: lifecycle only. Output bytes,
/// status lines, stats, and other per-frame churn never reach the log.
fn log_instance_event(instance: &InstanceId, event: &InstanceEvent) {
    match event {
        InstanceEvent::TabOpened { tab_id } => {
            tracing::info!(instance = %instance, tab = %tab_id, "engine tab opened")
        }
        InstanceEvent::TabClosed { tab_id } => {
            tracing::info!(instance = %instance, tab = %tab_id, "engine tab closed")
        }
        InstanceEvent::CommandFinished { tab_id, result } => match &result.error {
            None => tracing::info!(
                instance = %instance,
                tab = %tab_id,
                exit_code = result.exit_code,
                "engine command finished"
            ),
            Some(error) => tracing::warn!(
                instance = %instance,
                tab = %tab_id,
                exit_code = result.exit_code,
                code = %error.code,
                error = %error.message,
                "engine command failed"
            ),
        },
        InstanceEvent::Delta(InstanceDelta::SlotLaunched { tab_id, slot }) => tracing::info!(
            instance = %instance,
            tab = %tab_id,
            step = %slot.step_name,
            agent = %slot.agent_display_name,
            model = slot.model.as_deref().unwrap_or(""),
            "agent container launched"
        ),
        InstanceEvent::Delta(InstanceDelta::SlotContainerName {
            tab_id,
            container_name,
            ..
        }) => tracing::debug!(
            instance = %instance,
            tab = %tab_id,
            container = %container_name,
            "agent container running"
        ),
        InstanceEvent::Delta(InstanceDelta::SlotExited {
            tab_id, exit_code, ..
        }) => tracing::info!(
            instance = %instance,
            tab = %tab_id,
            exit_code = ?exit_code,
            "agent container exited"
        ),
        InstanceEvent::Delta(InstanceDelta::SlotStuck { tab_id, stuck, .. }) => tracing::info!(
            instance = %instance,
            tab = %tab_id,
            stuck,
            "agent stuck state changed"
        ),
        InstanceEvent::Delta(InstanceDelta::GroupStarted { tab_id }) => {
            tracing::info!(instance = %instance, tab = %tab_id, "parallel step group started")
        }
        InstanceEvent::Delta(InstanceDelta::GroupFinished { tab_id }) => {
            tracing::info!(instance = %instance, tab = %tab_id, "parallel step group finished")
        }
        InstanceEvent::DialogRaised { tab_id, request } if request.awaits_answer() => {
            tracing::debug!(instance = %instance, tab = %tab_id, "engine dialog awaiting an answer")
        }
        _ => {}
    }
}

fn parse_submit(
    catalogue: &'static CommandCatalogue,
    input: &SubmitInput,
) -> Result<ParsedCommandBoxInput, CommandError> {
    match input {
        SubmitInput::CommandBox { raw } => {
            Dispatch::<super::frontend::InstanceCommandFrontend>::parse_command_box_input(
                raw.trim(),
            )
        }
        SubmitInput::Argv { args } | SubmitInput::Lease { args, .. } => {
            // The longest catalogue path that prefixes the argv.
            let words: Vec<&str> = args
                .iter()
                .take_while(|a| !a.starts_with('-'))
                .map(String::as_str)
                .collect();
            let depth = (1..=words.len())
                .rev()
                .find(|n| catalogue.lookup_with_aliases(&words[..*n]).is_some())
                .ok_or_else(|| CommandError::unknown_command(&words))?;
            let path: Vec<String> = words[..depth].iter().map(|s| s.to_string()).collect();
            let rest: Vec<String> = args[depth..].to_vec();
            let refs: Vec<&str> = path.iter().map(String::as_str).collect();
            let parsed = catalogue.parse_raw_args(&refs, &rest)?;
            Ok(ParsedCommandBoxInput { path, args: parsed })
        }
    }
}

fn presenter_event(event: &InstanceEvent) -> Option<PresenterEvent> {
    Some(match event {
        InstanceEvent::TabOpened { tab_id } => PresenterEvent::TabOpened {
            tab_id: tab_id.clone(),
        },
        InstanceEvent::TabClosed { tab_id } => PresenterEvent::TabClosed {
            tab_id: tab_id.clone(),
        },
        InstanceEvent::Notice { level, text } => PresenterEvent::Notice {
            level: *level,
            text: text.clone(),
        },
        InstanceEvent::TerminalOpened {
            tab_id,
            terminal_id,
        } => PresenterEvent::TerminalOpened {
            tab_id: tab_id.clone(),
            terminal_id: terminal_id.clone(),
        },
        InstanceEvent::TerminalClosed {
            tab_id,
            terminal_id,
            exit_code,
        } => PresenterEvent::TerminalClosed {
            tab_id: tab_id.clone(),
            terminal_id: terminal_id.clone(),
            exit_code: *exit_code,
        },
        InstanceEvent::ContainerWindow {
            tab_id,
            slot_id,
            state,
        } => PresenterEvent::ContainerWindow {
            tab_id: tab_id.clone(),
            slot_id: *slot_id,
            state: *state,
        },
        InstanceEvent::ProgramStatus {
            tab_id, slot_id, ..
        } => PresenterEvent::ProgramStatus {
            tab_id: tab_id.clone(),
            slot_id: *slot_id,
        },
        InstanceEvent::Ended(_) => PresenterEvent::Ended,
        _ => return None,
    })
}

// ─── Frontend sessions ──────────────────────────────────────────────────────

/// One admitted frontend connection. Layer 3 feeds it every decoded inbound
/// frame and calls [`FrontendSession::disconnected`] when the socket closes
/// (dropping it does the same).
pub struct FrontendSession {
    host: Weak<EngineInstanceHost>,
    pub frontend_id: FrontendId,
    pub instance_id: InstanceId,
    pub role: FrontendRole,
    done: bool,
}

impl std::fmt::Debug for FrontendSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontendSession")
            .field("frontend_id", &self.frontend_id)
            .field("instance_id", &self.instance_id)
            .field("role", &self.role)
            .finish()
    }
}

impl FrontendSession {
    fn host(&self) -> Option<Arc<EngineInstanceHost>> {
        self.host.upgrade()
    }

    fn reply_error(&self, host: &EngineInstanceHost, tab_id: Option<TabId>, error: HostError) {
        let Some(hosted) = host.find(&self.instance_id) else {
            return;
        };
        let state = hosted.state();
        let frame = match tab_id {
            Some(tab_id) => EngineFrame::CommandFinished {
                tab_id,
                result: CommandResultWire {
                    exit_code: match &error {
                        HostError::Command(error) => i32::from(error.exit_code()),
                        _ => 1,
                    },
                    outcome: None,
                    error: Some(ErrorWire {
                        code: error.code().to_string(),
                        message: error.to_string(),
                    }),
                },
            },
            None => EngineFrame::Notice {
                level: MessageLevel::Error,
                text: error.to_string(),
            },
        };
        state.send_to(&self.frontend_id, frame);
    }

    /// Apply one decoded inbound frame. A protocol violation is returned so
    /// Layer 3 closes the socket; operation failures go back to this
    /// frontend as frames and keep the connection.
    pub async fn handle(&self, seq: Seq, frame: FrontendFrame) -> Result<(), ProtocolError> {
        let Some(host) = self.host() else {
            return Ok(());
        };
        let id = &self.instance_id;
        let me = &self.frontend_id;
        // A web secondary inherits the instance but cannot perform the
        // primary's close/quit actions. Native frontend behavior is unchanged.
        if self.role == FrontendRole::Secondary
            && matches!(
                &frame,
                FrontendFrame::Quit
                    | FrontendFrame::CloseTab { .. }
                    | FrontendFrame::OpenTab { .. }
                    | FrontendFrame::Submit { .. }
                    | FrontendFrame::DialogResponse { .. }
                    | FrontendFrame::Cancel { .. }
                    | FrontendFrame::CancelCountdown { .. }
                    | FrontendFrame::WorkflowRequest { .. }
                    | FrontendFrame::UiRequest(_)
                    | FrontendFrame::WorkerQueue {
                        request: crate::data::worker::queue::QueueRequest::Pick { .. }
                            | crate::data::worker::queue::QueueRequest::Checkout { .. }
                            | crate::data::worker::queue::QueueRequest::Finish { .. },
                        ..
                    }
            )
        {
            if let Some(hosted) = host.find(id) {
                let state = hosted.state();
                if state
                    .frontend(me)
                    .is_some_and(|f| f.summary.kind == FrontendKindWire::Web)
                {
                    state.send_to(
                        me,
                        EngineFrame::Notice {
                            level: MessageLevel::Error,
                            text: "only the primary frontend may change this instance".into(),
                        },
                    );
                    return Ok(());
                }
            }
        }
        match frame {
            FrontendFrame::WorkerQueue {
                request_id,
                tab_id,
                request,
            } => {
                use crate::data::worker::queue::{PickError, QueueReply};
                let context = host.find(id).and_then(|hosted| {
                    let state = hosted.state();
                    let instance = state.instance.clone()?;
                    let session_id = state.tab(&tab_id)?.session_id();
                    let session = instance
                        .session_manager
                        .get(&session_id)?
                        .try_read()
                        .ok()?
                        .clone();
                    Some((instance, session))
                });
                let reply = match context {
                    Some((instance, session)) => tokio::task::spawn_blocking(move || {
                        instance.queue_request(&session, request)
                    })
                    .await
                    .unwrap_or_else(|e| QueueReply::Error(PickError::Failed(e.to_string()))),
                    None => QueueReply::Error(PickError::Failed("no such instance tab".into())),
                };
                if let Some(hosted) = host.find(id) {
                    hosted
                        .state()
                        .send_to(me, EngineFrame::WorkerQueue { request_id, reply });
                }
            }
            FrontendFrame::Hello(_) => {
                return Err(ProtocolError::Malformed("duplicate hello".into()));
            }
            FrontendFrame::OpenTab { dir, options } => {
                if let Err(error) = host.open_tab(id, Some(me), dir, options) {
                    let detail = serde_json::to_string(&ErrorWire {
                        code: error.code().into(),
                        message: error.to_string(),
                    })
                    .unwrap();
                    self.reply_error(&host, None, error);
                    if let Some(hosted) = host.find(id) {
                        hosted.state().send_to(
                            me,
                            EngineFrame::UiAck {
                                request_seq: seq,
                                outcome: UiOutcome {
                                    detail: Some(detail),
                                    ..Default::default()
                                },
                            },
                        );
                    }
                }
            }
            FrontendFrame::CloseTab { tab_id } => {
                if let Err(error) = host.close_tab(id, &tab_id) {
                    self.reply_error(&host, None, error);
                }
            }
            FrontendFrame::FocusTab { tab_id } => {
                if let Some(hosted) = host.find(id) {
                    let mut state = hosted.state();
                    if state.tab(&tab_id).is_some() {
                        if let (Some(instance), Some(session)) = (
                            state.instance.clone(),
                            state.tab(&tab_id).map(TabRelay::session_id),
                        ) {
                            instance.set_active_tab_hint(&session);
                        }
                        state.active_tab_hint = Some(tab_id.clone());
                        state.broadcast(
                            &EngineFrame::Delta(InstanceDelta::ActiveTabHint { tab_id }),
                            Some(me),
                        );
                    }
                }
            }
            FrontendFrame::Submit {
                tab_id,
                input,
                pty_size,
            } => {
                if let Err(error) = host.submit(id, Some(me), &tab_id, input, pty_size) {
                    self.reply_error(&host, Some(tab_id), error);
                }
            }
            FrontendFrame::DialogResponse {
                dialog_id,
                response,
            } => {
                let by = AnsweredBy::Frontend {
                    frontend_id: me.clone(),
                };
                match host.answer(id, &dialog_id, response, by) {
                    Ok(DialogAnswerOutcome::Rejected) => self.reply_error(
                        &host,
                        None,
                        HostError::Command(CommandError::Other(
                            "that answer does not fit the dialog".into(),
                        )),
                    ),
                    Ok(_) | Err(HostError::UnknownDialog) => {}
                    Err(error) => self.reply_error(&host, None, error),
                }
            }
            FrontendFrame::Cancel { tab_id, repeat } => {
                let _ = host.with_relay(id, &tab_id, |relay| relay.cancel(repeat));
            }
            FrontendFrame::CancelCountdown { tab_id, step_name } => {
                let _ = host.with_relay(id, &tab_id, |relay| {
                    relay.cancel_countdown(step_name.as_deref())
                });
            }
            FrontendFrame::WorkflowRequest { tab_id, request } => {
                let _ = host.with_relay(id, &tab_id, |relay| relay.request_workflow(request));
            }
            FrontendFrame::Activity { tab_id, step_name } => {
                let _ = host.with_relay(id, &tab_id, |relay| {
                    relay.note_activity(step_name.as_deref())
                });
            }
            FrontendFrame::Resize(size) => {
                if let Some(hosted) = host.find(id) {
                    let mut state = hosted.state();
                    if let Some(record) = state
                        .frontends
                        .iter_mut()
                        .find(|f| &f.summary.frontend_id == me)
                    {
                        record.term_size = size;
                    }
                    // Later launches in every tab open at the new size (D8).
                    if self.role == FrontendRole::Primary {
                        let instance = state.instance.clone();
                        for relay in &mut state.tabs {
                            relay.set_pty_size((size.cols, size.rows));
                            if let Some(instance) = &instance {
                                instance.resize_tab(&relay.session_id(), (size.cols, size.rows));
                            }
                        }
                    }
                }
            }
            FrontendFrame::UiRequest(request) => {
                let outcome = match host.ui_request(id, Some(me.clone()), request).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let detail = serde_json::to_string(&ErrorWire {
                            code: error.code().into(),
                            message: error.to_string(),
                        })
                        .unwrap();
                        self.reply_error(&host, None, error);
                        UiOutcome {
                            detail: Some(detail),
                            ..Default::default()
                        }
                    }
                };
                if let Some(hosted) = host.find(id) {
                    hosted.state().send_to(
                        me,
                        EngineFrame::UiAck {
                            request_seq: seq,
                            outcome,
                        },
                    );
                }
            }
            FrontendFrame::Quit => {
                let _ = host.quit_instance(id, QuitReason::UserQuit);
            }
        }
        Ok(())
    }

    /// The socket closed. A primary gets the 2 s reconnect grace, then
    /// today's killed-TUI policy; a secondary changes nothing.
    pub fn disconnected(mut self) {
        self.disconnect();
    }

    fn disconnect(&mut self) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        if let Some(host) = self.host() {
            host.frontend_disconnected(&self.instance_id, &self.frontend_id);
        }
    }
}

impl Drop for FrontendSession {
    fn drop(&mut self) {
        self.disconnect();
    }
}

// ─── Layer 1 hook ───────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl EngineHost for EngineInstanceHost {
    fn activity(&self) -> EngineActivity {
        let mut activity = EngineActivity {
            presenter_holds: self.presenters.holds(),
            ..Default::default()
        };
        for hosted in self.list_instances() {
            let state = hosted.state();
            if state.lifecycle.ended().is_some() {
                continue;
            }
            activity.instances += 1;
            activity.frontends += state.lifecycle.connected_frontends();
            activity.reconnecting_frontends +=
                usize::from(state.lifecycle.reconnecting().is_some());
            activity.running_jobs += state.running_jobs();
        }
        activity
    }

    async fn drain(&self, reason: EngineStopReason) {
        self.draining.store(true, Ordering::SeqCst);
        // Running commands finish; then every frontend is detached.
        while self.running_jobs() > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.end_all(quit_reason(reason)).await;
    }

    async fn stop_now(&self, reason: EngineStopReason) {
        self.draining.store(true, Ordering::SeqCst);
        self.end_all(quit_reason(reason)).await;
    }

    fn resume(&self) {
        self.draining.store(false, Ordering::SeqCst);
    }

    fn status(&self) -> FeatureStatus {
        let activity = EngineHost::activity(self);
        FeatureStatus {
            running: activity.running_jobs,
            queued: 0,
            oldest_queued_age: None,
            detail: Some(serde_json::json!({
                "instances": activity.instances,
                "frontends": activity.frontends,
            })),
        }
    }

    fn apply_settings(&self, settings: &EngineSettings) {
        *self.settings.lock().unwrap_or_else(|p| p.into_inner()) = Some(settings.clone());
    }

    fn set_executor_share(&self, share: usize) {
        self.share.store(share, Ordering::SeqCst);
        if let Some(executor) = &self.executor {
            executor.set_share(crate::data::daemon_state::FeatureName::Engine, share);
        }
    }
}

fn quit_reason(reason: EngineStopReason) -> QuitReason {
    match reason {
        EngineStopReason::Disabled | EngineStopReason::IdleTimeout => QuitReason::EngineDisabled,
        EngineStopReason::DaemonShutdown => QuitReason::DaemonShutdown,
    }
}

impl std::fmt::Debug for EngineInstanceHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineInstanceHost")
            .field("instances", &self.instance_count())
            .finish_non_exhaustive()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl EngineInstanceHost {
    /// Bind a hand-built command to a tab, as `submit` binds a real one, so a
    /// test drives the command side of the channels itself.
    pub fn bind_test_command(
        &self,
        id: &InstanceId,
        tab_id: &TabId,
        submitted: super::SubmittedCommand,
    ) -> Arc<tokio::sync::Notify> {
        let hosted = self.find(id).expect("instance");
        let mut state = hosted.state();
        let relay = state.tab_mut(tab_id).expect("tab");
        let (_, mut events) = relay.reset_for_submit();
        events.extend(relay.bind(submitted));
        let mut outbox = Outbox::default();
        self.apply_events(&hosted, &mut state, events, &mut outbox);
        self.wake.clone()
    }

    /// The tab's command feeds, so a test plays the command side of them.
    pub fn test_tab_feeds(&self, id: &InstanceId, tab_id: &TabId) -> super::feeds::TabCommandFeeds {
        let hosted = self.find(id).expect("instance");
        let state = hosted.state();
        state.tab(tab_id).expect("tab").feeds().clone()
    }
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
