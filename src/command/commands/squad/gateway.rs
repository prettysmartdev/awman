//! The task gateway keeps squad commands identical for local daemon and
//! remote CLI/TUI callers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::data::ids::RunId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::command::commands::squad::commands::resolved_git_root;
use crate::command::commands::squad::runtime_guard::require_container_tier;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::config::env::DaemonEnvMap;
use crate::data::config::squad::SquadConfig;
use crate::data::fs::daemon_env::{env_overlay_names, EnvPersistence};
use crate::data::fs::task_store::{
    MountScope, Run, Task, TaskStatus, TaskStore, TaskUpdate, TaskWorkspace,
};
use crate::data::fs::SquadPaths;
use crate::data::repo_dockerfile_paths::RepoDockerfilePaths;
use crate::data::workflow_state::WorkflowState;
use crate::engine::container::naming::validate_task_slug;
use crate::engine::squad::env_state::{DaemonEnvState, RequiredEnvEntry};
use crate::engine::squad::SchedulerStatus;

/// The `--workspace` value selecting the durable per-task workspace. One
/// literal, shared by the catalogue default, Dispatch, and the remote gateway.
pub const DEFAULT_WORKSPACE_FLAG_VALUE: &str = "default";

const MIN_INTERVAL_SECS: u64 = 60;
const MAX_INTERVAL_SECS: u64 = 86_400;

/// How many runs `squad show` carries back. One number, used by the local
/// gateway when it builds the response and by the remote gateway when it
/// truncates one, so both sides agree without a second flag on the wire.
pub const DEFAULT_RUN_HISTORY_LIMIT: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTask {
    pub name: String,
    pub description: String,
    /// Which workspace the task is bound to. The gateway resolves this, once,
    /// into the persisted `repo_scope` + `mount_scope` pair — see
    /// [`Task`]'s type-level documentation for the resulting semantics.
    pub workspace: TaskWorkspace,
    /// The requested scope *within* a custom git repository. Ignored (and
    /// overridden with [`MountScope::Directory`]) whenever the resolved
    /// effective root is not a git repository, because there is then no git
    /// root for `cwd` and `gitroot` to differ about.
    pub mount_scope: MountScope,
    /// The scheduler stores whole seconds; the wire field is milliseconds and
    /// therefore currently accepts values divisible by 1000 only.
    #[serde(rename = "interval_ms", with = "seconds_as_ms")]
    pub interval_secs: u64,
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Raw overlay specs in `--overlay` syntax. Validated for *syntax* here at
    /// creation time; host-side existence is deliberately not checked, since
    /// the host's state at creation differs from its state at each future run
    /// (`docs/08-overlays.md`).
    #[serde(default)]
    pub overlays: Vec<String>,
    /// The task's own agent pool, written to `tasks/<name>/config.json` as a
    /// `squad.agentsToModels` block (WI 0110). Empty means "no task config":
    /// the task inherits the global `squad` block whole.
    #[serde(default)]
    pub agents_to_models: BTreeMap<String, Vec<String>>,
    /// The task's workflow prerequisite policy (WI 0149): decisions only, never
    /// a credential value. `None` means "not set", which headless resolves to
    /// deny.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisites: Option<crate::data::config::workflows::PrerequisitePolicy>,
}

/// The fields `squad edit` may change on an existing task (WI 0110).
///
/// Every field is "leave alone" when absent. The two nullable ones use
/// `Option<Option<_>>` so an edit can distinguish *not mentioned* from
/// *cleared back to the squad default*, which a bare `Option` could not.
///
/// Absent by design: `name` (the task's identity, its data directory, and its
/// container names), `workspace` and `mount_scope` (capture-once isolation
/// decisions — see [`Task`]'s type-level docs). Changing those means creating a
/// different task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateTask {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        default,
        rename = "interval_ms",
        with = "optional_seconds_as_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub interval_secs: Option<u64>,
    /// `None` leaves the agent alone; `Some(None)` clears it back to the squad
    /// default; `Some(Some(a))` sets it.
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub agent: Option<Option<String>>,
    /// As [`agent`](Self::agent), for the model.
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub model: Option<Option<String>>,
    /// Replaces the whole overlay list when given (an empty vector clears it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlays: Option<Vec<String>>,
    /// Replaces the task's `config.json` agent pool when given; an empty map
    /// removes the task config so the task inherits the global block again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_to_models: Option<BTreeMap<String, Vec<String>>>,
    /// Replaces the task's workflow prerequisite policy when given (WI 0149).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisites: Option<crate::data::config::workflows::PrerequisitePolicy>,
}

impl UpdateTask {
    /// Whether this request would change nothing. `squad edit` refuses such a
    /// request rather than writing an update whose only effect is bumping
    /// `updated_at`.
    pub fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.interval_secs.is_none()
            && self.agent.is_none()
            && self.model.is_none()
            && self.overlays.is_none()
            && self.agents_to_models.is_none()
            && self.prerequisites.is_none()
    }

    /// The subset of this request the task *row* carries. `agents_to_models`
    /// is deliberately absent: it lives in the task's `config.json`, not in
    /// the database, so the store never learns about it.
    fn to_store_update(&self) -> TaskUpdate {
        TaskUpdate {
            prerequisites: self.prerequisites.clone(),
            description: self.description.clone(),
            interval_secs: self.interval_secs,
            agent: self.agent.clone(),
            model: self.model.clone(),
            overlays: self.overlays.clone(),
        }
    }
}

/// Parse `--agent-models` specs (`<agent>=<model>[,<model>…]`) into the
/// `agentsToModels` map they describe (WI 0110).
///
/// One spec per agent; repeating an agent merges its models, preserving order
/// and dropping duplicates, so `--agent-models claude=a --agent-models claude=b`
/// and `--agent-models claude=a,b` mean the same thing.
pub fn parse_agent_models_specs(specs: &[String]) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for spec in specs {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        let (agent, models) = spec.split_once('=').ok_or_else(|| {
            format!("agent-models spec {spec:?} must be written <agent>=<model>[,<model>...]")
        })?;
        let agent = agent.trim();
        if agent.is_empty() {
            return Err(format!("agent-models spec {spec:?} names no agent"));
        }
        let entry = map.entry(agent.to_string()).or_default();
        for model in models.split(',') {
            let model = model.trim();
            if model.is_empty() {
                continue;
            }
            if !entry.iter().any(|existing| existing == model) {
                entry.push(model.to_string());
            }
        }
    }
    Ok(map)
}

/// Render an `agentsToModels` map back into `--agent-models` spec strings, so
/// the remote gateway can send over the wire exactly what the CLI parses.
pub fn format_agent_models_specs(map: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    map.iter()
        .map(|(agent, models)| format!("{agent}={}", models.join(",")))
        .collect()
}

/// The `squad show` response: one task plus its recent run history.
///
/// Both gateways speak this one shape, so `get` and `runs` cannot drift apart
/// between the daemon-local and HTTP paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetail {
    #[serde(with = "crate::command::daemon_api::squad::task_wire")]
    pub task: Task,
    #[serde(with = "crate::command::daemon_api::squad::run_list_wire")]
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub bound_addr: Option<String>,
    pub task_count: usize,
    pub active_count: usize,
    #[serde(default, with = "crate::data::wire::opt_timestamp")]
    pub last_tick: Option<DateTime<Utc>>,
    pub in_flight: usize,
    /// A live scheduler reload failure; absent for healthy or older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
    /// Where the daemon persists its payload environment (WI 0116 §5a):
    /// `"keychain"`, `"none"`, or `"unavailable(<reason>)"`.
    ///
    /// Reported here so `awman squad status` can name a degraded daemon
    /// without anyone reading a log file — it appends `; env persistence
    /// <state>` for exactly the `unavailable(...)` case, since `keychain` is
    /// the healthy default and `none` is the user's own opt-out. A string
    /// rather than the typed [`EnvPersistence`] because the `unavailable(...)`
    /// reason is open-ended and every consumer only ever displays it.
    ///
    /// Deliberately *not* on `Task`, so it can never reach the TUI indicator's
    /// `classify`: a keychain that stopped working is not a task-health
    /// problem, and colouring the squad dot for it would bury the states that
    /// are (CONTRACT §5).
    ///
    /// [`EnvPersistence`]: crate::data::fs::daemon_env::EnvPersistence
    /// `None` when the answer did not come from a live daemon — the pidfile
    /// sidecar cannot know it.
    #[serde(default)]
    pub env_persistence: Option<EnvPersistence>,
    /// Required env names the daemon has no value for, sorted. Empty on the
    /// common path, which is why the CLI one-liner only mentions it when it is
    /// not.
    #[serde(default)]
    pub unmet_env: Vec<String>,
}

/// The daemon's `required_env`, with a salted digest per name it holds.
///
/// This is the *check* every squad command performs and almost none act on: a
/// client compares each digest against one it computes from its own value and
/// sends nothing when they match. No value is ever in this response.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct EnvCoverage {
    /// The daemon's per-lifetime salt, 64 lowercase hex characters.
    ///
    /// It makes digests incomparable across daemons and machines, and rotating
    /// it per lifetime stops a restarted daemon having stale digests trusted
    /// against it. It does *not* protect the digests from the caller that
    /// receives this response, which holds both — see `env_state`'s module
    /// documentation for what that does and does not mean.
    pub salt: String,
    /// [`DaemonStatus::env_persistence`]'s value, so `awman squad env` needs
    /// only this one call.
    pub persistence: EnvPersistence,
    pub required: Vec<RequiredEnvEntry>,
}

/// Hand-written so no future `debug!(?coverage)` can put the salt in a log —
/// the same defence [`DaemonEnvMap`] and `Salt` already have. The entries carry
/// digests and names, never values, so they print in full.
impl std::fmt::Debug for EnvCoverage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvCoverage")
            .field("salt", &"<redacted>")
            .field("persistence", &self.persistence)
            .field("required", &self.required)
            .finish()
    }
}

/// One push: the three states a client can report per required name.
///
/// `Debug` is derived and still safe — [`DaemonEnvMap`]'s own `Debug` prints
/// names only — which is the point of the type. This body deliberately does not
/// travel through the `{subcommand, args}` envelope of `/v1/worker/commands`:
/// CLI-arg-shaped strings drift into tracing spans and error text, a typed body
/// with a redacting `Debug` does not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvPush {
    /// Names the client has. Each replaces whatever the daemon holds.
    #[serde(default)]
    pub vars: DaemonEnvMap,
    /// Names the client was asked for and cannot supply. **Never a deletion**:
    /// "I don't have it" is not "nobody should have it".
    #[serde(default)]
    pub absent: Vec<String>,
}

/// What a push is answered with: names only, and the fresh coverage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvPushResponse {
    /// Names whose value the daemon took.
    pub accepted: Vec<String>,
    /// Names in `vars` the daemon did not take because they are not in
    /// `required_env`. An empty value is *not* listed here — it counts as
    /// absent, which is reported nowhere.
    pub ignored: Vec<String>,
    pub coverage: EnvCoverage,
}

#[async_trait]
pub trait TaskGateway: Send + Sync {
    /// Allocate the run accepted by the resource API. Implementations without a scheduler refuse it.
    async fn create_run(&self, _name: &str) -> Result<RunId, CommandError> {
        Err(CommandError::Other("run creation unavailable".into()))
    }
    async fn cancel_run_id(&self, _id: &RunId) -> Result<(), CommandError> {
        Err(CommandError::Other("run cancellation unavailable".into()))
    }

    async fn create(&self, req: CreateTask) -> Result<Task, CommandError>;
    /// Apply an edit to an existing task, returning it as it now stands.
    async fn update(&self, name: &str, req: UpdateTask) -> Result<Task, CommandError>;
    async fn list(&self) -> Result<Vec<Task>, CommandError>;
    async fn get(&self, name: &str) -> Result<Task, CommandError>;
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError>;
    async fn set_status(&self, name: &str, status: TaskStatus) -> Result<(), CommandError>;
    /// Ask for `name` to be evaluated on the next scheduler tick, whatever its
    /// interval and backoff would otherwise say.
    async fn trigger(&self, name: &str) -> Result<(), CommandError>;
    /// Stop `name`'s in-progress run: its evaluation is abandoned, every
    /// container it started is stopped, and the run is recorded as
    /// `canceled`. An error when the task has no run in progress.
    async fn cancel(&self, name: &str) -> Result<(), CommandError>;
    async fn delete(&self, name: &str) -> Result<(), CommandError>;
    async fn status(&self) -> Result<DaemonStatus, CommandError>;
    /// Read the state of the task's currently running workflow, if any.
    ///
    /// The default keeps lightweight test gateways source-compatible; real
    /// gateways implement the transport-specific lookup below.
    async fn workflow_state(&self, _task: &str) -> Result<Option<WorkflowState>, CommandError> {
        Ok(None)
    }

    /// The daemon's `required_env` with a salted digest per held name (§4a).
    ///
    /// Defaulted so the many small test gateways in this tree stay
    /// source-compatible; an empty coverage plans no push, which is the correct
    /// behaviour for a gateway that has no daemon behind it.
    async fn env_coverage(&self) -> Result<EnvCoverage, CommandError> {
        Ok(EnvCoverage::default())
    }

    /// Merge a client's values into the daemon's overlay (§4b).
    async fn push_env(&self, _push: EnvPush) -> Result<EnvPushResponse, CommandError> {
        Ok(EnvPushResponse::default())
    }

    /// Remove the persisted keychain item (§5c). `false` when there is no
    /// keychain backend to clear.
    async fn clear_env_store(&self) -> Result<bool, CommandError> {
        Ok(false)
    }
}

/// Boxable adaptor for Dispatch's shared daemon gateway handle.
pub struct SharedTaskGateway(pub Arc<dyn TaskGateway>);

#[async_trait]
impl TaskGateway for SharedTaskGateway {
    async fn create_run(&self, name: &str) -> Result<RunId, CommandError> {
        self.0.create_run(name).await
    }
    async fn cancel_run_id(&self, id: &RunId) -> Result<(), CommandError> {
        self.0.cancel_run_id(id).await
    }

    async fn create(&self, req: CreateTask) -> Result<Task, CommandError> {
        self.0.create(req).await
    }
    async fn update(&self, name: &str, req: UpdateTask) -> Result<Task, CommandError> {
        self.0.update(name, req).await
    }
    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        self.0.list().await
    }
    async fn get(&self, name: &str) -> Result<Task, CommandError> {
        self.0.get(name).await
    }
    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        self.0.runs(name, limit).await
    }
    async fn set_status(&self, name: &str, status: TaskStatus) -> Result<(), CommandError> {
        self.0.set_status(name, status).await
    }
    async fn trigger(&self, name: &str) -> Result<(), CommandError> {
        self.0.trigger(name).await
    }
    async fn cancel(&self, name: &str) -> Result<(), CommandError> {
        self.0.cancel(name).await
    }
    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        self.0.delete(name).await
    }
    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        self.0.status().await
    }
    async fn workflow_state(&self, task: &str) -> Result<Option<WorkflowState>, CommandError> {
        self.0.workflow_state(task).await
    }
    async fn env_coverage(&self) -> Result<EnvCoverage, CommandError> {
        self.0.env_coverage().await
    }
    async fn push_env(&self, push: EnvPush) -> Result<EnvPushResponse, CommandError> {
        self.0.push_env(push).await
    }
    async fn clear_env_store(&self) -> Result<bool, CommandError> {
        self.0.clear_env_store().await
    }
}

/// Daemon-only gateway. All persistent-task validation belongs here.
pub struct LocalTaskGateway {
    store: Arc<TaskStore>,
    engines: Engines,
    status: Arc<Mutex<SchedulerStatus>>,
    /// The daemon's own squad root, so the durable workspace this gateway
    /// creates at task creation is the same directory the scheduler later
    /// seeds and mounts. Re-deriving it from the process environment here
    /// would let the two disagree whenever the daemon was started with an
    /// explicit root.
    paths: SquadPaths,
    /// The daemon's payload-environment metadata (WI 0116 §4). Shared with the
    /// scheduler, which reads the same coverage when it opens a run row.
    env_state: Arc<DaemonEnvState>,
    /// The daemon's environment, captured when the gateway is built (at
    /// bootstrap), so agent definitions and trust resolve against the same
    /// data home the scheduler uses for every later request.
    env: crate::data::config::env::EnvSnapshot,
}

impl LocalTaskGateway {
    pub fn new(
        store: Arc<TaskStore>,
        engines: Engines,
        status: Arc<Mutex<SchedulerStatus>>,
        paths: SquadPaths,
        env_state: Arc<DaemonEnvState>,
    ) -> Self {
        Self {
            store,
            engines,
            status,
            paths,
            env_state,
            // The daemon owns no session; its bootstrap snapshot is the
            // process. A host that already holds one passes it via `with_env`.
            env: crate::data::config::env::Env::from_process(),
        }
    }

    /// Resolve against `env` (the daemon core's snapshot) instead of a fresh
    /// process capture.
    pub fn with_env(mut self, env: crate::data::config::env::EnvSnapshot) -> Self {
        self.env = env;
        self
    }

    /// Recompute `required_env` from the task store and the daemon's own
    /// configuration.
    ///
    /// Deliberately **not cached**: it is computed on demand so it tracks tasks
    /// being added and removed, which is also what garbage-collects a value
    /// whose last declaring task has gone (§4b). The union is
    ///
    /// * every `env(NAME)` across the task store, attributed to the task that
    ///   declares it;
    /// * every `env(NAME)` in the daemon's own global config and
    ///   `AWMAN_OVERLAYS`, attributed to *every* task because those overlays
    ///   apply to every run.
    ///
    /// Nothing else. No name is added on the daemon's own behalf, so every
    /// required name is one something declared and every one is reported unmet
    /// on the same terms.
    ///
    /// `TaskStore` stays daemon-only, so a client never reads any of this — it
    /// asks for the answer over the socket.
    pub async fn refresh_required_env(&self) -> Result<(), CommandError> {
        let tasks = self.store.list()?;
        self.refresh_required_env_from(&tasks).await;
        Ok(())
    }

    /// [`Self::refresh_required_env`] over a task list the caller already has.
    ///
    /// `list` and `status` both read every task anyway, and the indicator
    /// poller calls one of them every ten seconds; re-reading the store here
    /// would double that query for no new information.
    ///
    /// The `set_required` half runs on the blocking pool: garbage-collecting a
    /// name rewrites the keychain item, and that call is capped at five
    /// seconds. Parking a runtime worker for five seconds on a locked or
    /// D-Bus-less keychain would stop `/v1/status` answering — the endpoint the
    /// indicator poller and `awman squad status` both need — and an
    /// authenticated client can drive it. The bootstrap already treats the
    /// store this way; the request path must too.
    async fn refresh_required_env_from(&self, tasks: &[Task]) {
        let required = self.required_env_map(tasks);
        let state = Arc::clone(&self.env_state);
        let now = crate::data::wire::now();
        if let Err(error) =
            tokio::task::spawn_blocking(move || state.set_required(required, now)).await
        {
            tracing::debug!(error = %error, "squad env: coverage refresh task failed");
        }
    }

    /// Which names the daemon requires, and which tasks ask for each. Pure map
    /// building over an already-loaded task list plus the daemon-wide sources.
    fn required_env_map(&self, tasks: &[Task]) -> BTreeMap<String, Vec<String>> {
        let mut task_names: Vec<String> = tasks.iter().map(|task| task.name.clone()).collect();
        task_names.sort();

        let mut required: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for task in tasks {
            for name in env_overlay_names(&task.overlays) {
                required.entry(name).or_default().push(task.name.clone());
            }
            for name in self.custom_auth_env_names(task) {
                required.entry(name).or_default().push(task.name.clone());
            }
            // The token variable a task's prerequisite policy grants (WI 0149) is
            // read from the same daemon env store, so it is required on the same
            // terms and the "daemon has no value for" warning covers it.
            for name in prerequisite_token_env_names(task) {
                required.entry(name).or_default().push(task.name.clone());
            }
        }
        for name in self.daemon_wide_env_names() {
            required.insert(name, task_names.clone());
        }
        required
    }

    /// `auth.env` names of the approved custom agents `task` may launch: its
    /// own agent and every agent in its effective pool (task file over global).
    /// Forwarding reads them from the same daemon overlay as an `env(NAME)`
    /// overlay, so they are required on the same terms. Resolution enforces
    /// trust: an unapproved, invalid or deleted definition contributes no
    /// name, so the daemon never solicits a secret on its behalf. First-party
    /// agents keep their own credential paths and add nothing here.
    fn custom_auth_env_names(&self, task: &Task) -> Vec<String> {
        task_custom_auth_env_names(
            &self.env,
            &self.paths,
            SquadConfig::from_global(&self.engines.global_config),
            task,
        )
    }

    /// `env(NAME)` from the daemon's own overlay sources — its global config
    /// and `AWMAN_OVERLAYS` — which apply to every run rather than to one task.
    ///
    /// A config that fails to load contributes nothing rather than failing the
    /// request: coverage is advisory, and a malformed global file already
    /// surfaces loudly on the scheduler's own tick.
    fn daemon_wide_env_names(&self) -> Vec<String> {
        let mut specs: Vec<String> = Vec::new();
        specs.extend(
            self.engines
                .global_config
                .overlays
                .clone()
                .unwrap_or_default(),
        );
        // `AWMAN_OVERLAYS` is one comma-separated string of specs; `env(NAME)`
        // never contains a comma, so splitting first is lossless.
        if let Some(raw) =
            crate::data::config::env::host_var(crate::data::config::env::AWMAN_OVERLAYS)
        {
            specs.extend(raw.split(',').map(|spec| spec.trim().to_string()));
        }
        env_overlay_names(&specs)
    }

    /// Stamp the derived `unmet_env` marker onto tasks on their way out.
    ///
    /// One pass over an already-loaded list: the indicator poller's single
    /// `list` call stays a single call, exactly as `last_run_status` riding on
    /// the task row keeps it one.
    fn with_unmet_env(&self, mut tasks: Vec<Task>) -> Vec<Task> {
        for task in &mut tasks {
            task.unmet_env = self.env_state.unmet_for_task(task);
        }
        tasks
    }

    fn with_unmet_env_one(&self, mut task: Task) -> Task {
        task.unmet_env = self.env_state.unmet_for_task(&task);
        task
    }

    /// Recompute coverage, logging rather than failing when the store cannot be
    /// read. A command must not fail because the daemon could not refresh an
    /// advisory list.
    async fn refresh_required_env_best_effort(&self) {
        if let Err(error) = self.refresh_required_env().await {
            tracing::debug!(error = %error, "squad env: could not refresh required_env");
        }
    }

    /// The coverage response, computed fresh.
    fn coverage(&self) -> EnvCoverage {
        EnvCoverage {
            salt: self.env_state.salt().to_hex(),
            persistence: self.env_state.persistence(),
            required: self.env_state.entries(),
        }
    }

    /// Validate everything that must hold before a task is persisted, and
    /// resolve the requested workspace into the effective root + scope pair
    /// the task will carry for its whole lifetime.
    ///
    /// Nothing here writes to the store *or the filesystem*, so a rejection at
    /// any step leaves neither a partial task nor a stray directory behind.
    /// The durable workspace is only created once every check has passed —
    /// see [`Self::ensure_durable_workspace`], called from `create`.
    fn validate_create(&self, req: &CreateTask) -> Result<ResolvedWorkspace, CommandError> {
        validate_task_slug(&req.name)?;
        if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&req.interval_secs) {
            return Err(CommandError::Other(format!(
                "task interval must be between {MIN_INTERVAL_SECS} and {MAX_INTERVAL_SECS} seconds"
            )));
        }
        // Syntax-only overlay validation, before any store write: a malformed
        // spec is rejected at creation rather than discovered at first run.
        for spec in &req.overlays {
            crate::command::commands::parse_overlay_list(spec).map_err(|reason| {
                CommandError::InvalidOverlaySpec {
                    spec: spec.clone(),
                    reason,
                }
            })?;
        }

        let resolved = self.resolve_workspace(req)?;

        let mut agents = BTreeSet::new();
        if let Some(agent) = &req.agent {
            agents.insert(agent.clone());
        }
        // The task's own pool counts as much as the global one: the leader
        // picks its workflow's step agents from whichever pool applies, and
        // every one of them needs a Dockerfile in the repository (WI 0110).
        agents.extend(req.agents_to_models.keys().cloned());
        if let Some(pool) = SquadConfig::from_global(&self.engines.global_config).agents_to_models {
            agents.extend(pool.into_keys());
        }
        // Agent Dockerfiles are discovered from the repository a task is bound
        // to. A directory-workspace task has no repository, so there is nothing
        // to validate against — a missing agent image surfaces at launch, the
        // same way it does for any other non-repo-rooted run.
        if let Some(git_root) = &resolved.git_root {
            validate_task_agents(&self.env, git_root, &agents)?;
        }
        Ok(resolved)
    }

    /// Resolve `workspace` + `mount_scope` into the task's effective root and
    /// the scope actually stored, applying the WI-0106 rules:
    ///
    /// * **Default Task Workspace** → the durable
    ///   `~/.awman/squad/tasks/<name>/workspace/` directory, created here, with
    ///   [`MountScope::Directory`] (a plain directory: mounted whole, never
    ///   worktree-isolated).
    /// * **Custom Folder / Repo that does not exist** → a hard error. Unlike
    ///   the default workspace, an arbitrary user-entered path is never
    ///   silently created.
    /// * **Custom Folder / Repo that IS a git root** → the requested
    ///   `cwd`/`gitroot` scope is kept (both name the root itself), and every
    ///   run is worktree-isolated.
    /// * **Custom Folder / Repo that is not a git root** — a plain directory,
    ///   or a subdirectory of a repository — → kept as the effective root with
    ///   [`MountScope::Directory`]; the user chose that folder precisely
    ///   because they want it used and preserved, so it is treated as no less
    ///   durable than the default workspace and is never widened to the
    ///   enclosing repository by a worktree.
    fn resolve_workspace(&self, req: &CreateTask) -> Result<ResolvedWorkspace, CommandError> {
        // Resolved, not created: nothing touches the filesystem until every
        // validation has passed, so a rejected creation leaves nothing behind.
        let durable = self.paths.task_dir(&req.name)?;
        let requested = match &req.workspace {
            TaskWorkspace::Default => {
                return Ok(ResolvedWorkspace {
                    repo_scope: durable.clone(),
                    mount_scope: MountScope::Directory,
                    git_root: None,
                    durable_workspace: durable,
                });
            }
            TaskWorkspace::Custom(path) => path.clone(),
        };

        // A path that does not exist is a hard error: there is nothing to
        // mount, and creating it would silently invent a workspace the user
        // did not ask for.
        let canonical = std::fs::canonicalize(&requested).map_err(|error| {
            CommandError::Other(format!(
                "task workspace {} does not exist or cannot be resolved: {error}",
                requested.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(CommandError::Other(format!(
                "task workspace {} is not a directory",
                canonical.display()
            )));
        }
        // `GitEngine::resolve_root` — the same detector the interview's
        // warning and every run's `open_session` use — decides whether this
        // path is a repository root. A second, weaker `.git`-exists probe
        // could classify a malformed marker as a repository here and then
        // fail at launch, so there is exactly one answer.
        //
        // The comparison is deliberately against the root *itself*, not merely
        // "is inside a repository". A subdirectory of a repository is exactly
        // the "not a git root" case the interview warns about and the user
        // chose to keep, so it is bound and mounted as the plain directory it
        // is. Treating it as repository-backed would force worktree isolation,
        // and a worktree is a checkout of the whole repository — which would
        // silently widen the run's view from the folder the user picked to its
        // entire enclosing repository, exactly the widening `mount_scope`'s
        // capture-once semantics exist to prevent.
        match resolved_git_root(&self.engines.git_engine, &canonical) {
            Some(git_root) if git_root == canonical => Ok(ResolvedWorkspace {
                repo_scope: canonical,
                // The chosen path *is* the root here, so `cwd` and `gitroot`
                // name the same directory: the captured answer is stored
                // verbatim and neither can widen anything.
                mount_scope: req.mount_scope,
                git_root: Some(git_root),
                durable_workspace: durable,
            }),
            // Not a repository root: no worktree, direct mount, and the same
            // durability expectation as the default workspace.
            _ => Ok(ResolvedWorkspace {
                repo_scope: canonical,
                mount_scope: MountScope::Directory,
                git_root: None,
                durable_workspace: durable,
            }),
        }
    }

    /// Create (idempotently) the durable per-task workspace.
    ///
    /// Every task gets one, whichever workspace it is bound to: a
    /// custom-directory task's runs still mount this directory as a context
    /// overlay so task-scoped persistent data survives across runs regardless
    /// of workspace mode. It is created once and never deleted, emptied, or
    /// replaced until the task itself is removed.
    ///
    /// The path came from `SquadPaths::task_dir`, which validates the
    /// user-influenced name against the tasks root before appending the fixed
    /// `workspace` leaf, so a crafted name cannot escape.
    fn ensure_durable_workspace(dir: &Path) -> Result<(), CommandError> {
        crate::data::fs::TaskStore::ensure_workspace(dir).map_err(CommandError::Data)
    }

    /// Write (or remove) a task's own `config.json` (WI 0110).
    ///
    /// An empty pool removes the file rather than writing an empty block, so
    /// "no task config" has exactly one on-disk representation and a task that
    /// gives up its pool goes back to inheriting the global one cleanly.
    ///
    /// The document written is a task-scope config file carrying only
    /// `workflows.dynamic.agentsToModels`, so the file the daemon writes and the file a user may
    /// hand-edit are the same shape, parsed by the same loader.
    fn write_task_config(
        &self,
        name: &str,
        agents_to_models: &BTreeMap<String, Vec<String>>,
    ) -> Result<(), CommandError> {
        let path = self.paths.task_config_file(name)?;
        if agents_to_models.is_empty() {
            return crate::data::fs::TaskStore::remove_config(&path).map_err(CommandError::Data);
        }
        let squad = SquadConfig {
            agents_to_models: Some(
                agents_to_models
                    .iter()
                    .map(|(agent, models)| (agent.clone(), models.clone()))
                    .collect(),
            ),
            ..Default::default()
        };
        // Validated before it is written, with the same rules the global file
        // is held to, so the daemon can never author a task file its own
        // loader would later reject.
        squad.validate()?;
        let document = squad.to_task_file();
        crate::data::fs::TaskStore::write_config(&path, &document).map_err(CommandError::Data)
    }
}

/// The effective root a task will be bound to, as resolved once at creation.
#[derive(Debug, Clone)]
pub struct ResolvedWorkspace {
    /// The task's effective root — what gets persisted as `Task::repo_scope`.
    pub repo_scope: PathBuf,
    /// The scope actually persisted, which also decides worktree usage.
    pub mount_scope: MountScope,
    /// The enclosing git root, when the effective root is inside one. `None`
    /// means the effective root is a plain directory.
    pub git_root: Option<PathBuf>,
    /// The durable `tasks/<name>/workspace/` directory, always created.
    pub durable_workspace: PathBuf,
}

#[async_trait]
impl TaskGateway for LocalTaskGateway {
    async fn create_run(&self, name: &str) -> Result<RunId, CommandError> {
        let task = self.get(name).await?;
        let mut status = self.status.lock().expect("scheduler status mutex");
        if let Some(id) = status.pending_runs.get(task.id.as_str()) {
            return Ok(id.clone());
        }
        if let Some(run) = self.store.running_run_for(&task.id)? {
            return Ok(run.id);
        }
        let unmet = self.env_state.unmet_for_task(&task);
        let id = self
            .store
            .start_run_with_env(&task.id, None, crate::data::wire::now(), &unmet)?;
        // The scheduler consumes this already allocated run on its next tick.
        // A daemon restart records it interrupted through normal orphan recovery.
        status.pending_runs.insert(task.id.to_string(), id.clone());
        status.cancellations.insert(
            task.id.to_string(),
            crate::engine::squad::scheduler::RunCancellation::new(id.clone()),
        );
        Ok(id)
    }
    async fn cancel_run_id(&self, id: &RunId) -> Result<(), CommandError> {
        let mut status = self.status.lock().expect("scheduler status mutex");
        let pending = status
            .pending_runs
            .iter()
            .find(|(_, run)| *run == id)
            .map(|(task, _)| task.clone());
        if let Some(task) = pending {
            status.pending_runs.remove(&task);
            status.cancellations.remove(&task);
            self.store.finish_run(
                id,
                crate::data::fs::task_store::RunStatus::Canceled,
                &Default::default(),
                crate::data::wire::now(),
            )?;
            return Ok(());
        }
        if let Some(cancel) = status
            .cancellations
            .values()
            .find(|cancel| &cancel.run_id == id)
        {
            cancel.cancel();
            return Ok(());
        }
        Err(CommandError::Other("run not found".into()))
    }

    async fn create(&self, req: CreateTask) -> Result<Task, CommandError> {
        tracing::info!(task = %req.name, "squad administrator requested task creation");
        // Must remain first: changing a runtime must never mutate existing
        // task state before the sandbox refusal is reported.
        require_container_tier(&self.engines)?;
        let resolved = self.validate_create(&req)?;
        // Only now, with every check passed, does anything reach the disk.
        Self::ensure_durable_workspace(&resolved.durable_workspace)?;
        self.write_task_config(&req.name, &req.agents_to_models)?;
        let now = crate::data::wire::now();
        let task = Task {
            prerequisites: req.prerequisites.clone(),
            id: crate::data::ids::TaskId::new(),
            name: req.name,
            description: req.description,
            repo_scope: resolved.repo_scope,
            mount_scope: resolved.mount_scope,
            overlays: req.overlays,
            interval_secs: req.interval_secs,
            status: TaskStatus::Active,
            agent: req.agent,
            model: req.model,
            backoff_until: None,
            created_at: now,
            updated_at: now,
            last_run_at: None,
            trigger_requested_at: None,
            last_run_status: None,
            unmet_env: Vec::new(),
        };
        self.store.create(&task)?;
        // The new task may have brought a new `env()` name with it, so the
        // required set is recomputed before the marker is derived — this is the
        // earliest and most actionable moment to tell the user (§6a).
        self.refresh_required_env_best_effort().await;
        let task = self.with_unmet_env_one(task);
        tracing::info!(
            task = %task.name,
            repo_scope = %task.repo_scope.display(),
            mount_scope = ?task.mount_scope,
            worktree = task.uses_worktree(),
            durable_workspace = %resolved.durable_workspace.display(),
            overlays = task.overlays.len(),
            "squad task created"
        );
        Ok(task)
    }

    async fn update(&self, name: &str, req: UpdateTask) -> Result<Task, CommandError> {
        tracing::info!(task = %name, "squad administrator requested task edit");
        require_container_tier(&self.engines)?;
        let existing = self
            .store
            .get(name)?
            .ok_or_else(|| CommandError::Other(format!("task {name:?} was not found")))?;

        // The same rules creation enforces, applied to whichever fields the
        // edit actually carries — an edit must not be able to install a value
        // `squad add` would have rejected.
        if let Some(interval) = req.interval_secs {
            if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&interval) {
                return Err(CommandError::Other(format!(
                    "task interval must be between {MIN_INTERVAL_SECS} and {MAX_INTERVAL_SECS} seconds"
                )));
            }
        }
        if let Some(overlays) = &req.overlays {
            for spec in overlays {
                crate::command::commands::parse_overlay_list(spec).map_err(|reason| {
                    CommandError::InvalidOverlaySpec {
                        spec: spec.clone(),
                        reason,
                    }
                })?;
            }
        }
        // Agent Dockerfiles are only discoverable for a repository-backed
        // task, exactly as at creation; a directory-workspace task's images
        // are scaffolded on its first run instead.
        if existing.mount_scope.is_git_repo() {
            let mut agents = BTreeSet::new();
            match &req.agent {
                Some(Some(agent)) => {
                    agents.insert(agent.clone());
                }
                Some(None) => {}
                None => {
                    if let Some(agent) = &existing.agent {
                        agents.insert(agent.clone());
                    }
                }
            }
            if let Some(pool) = &req.agents_to_models {
                agents.extend(pool.keys().cloned());
            }
            validate_task_agents(&self.env, &existing.repo_scope, &agents)?;
        }

        // The task config is written before the row, so a failed config write
        // leaves the task exactly as it was rather than half-edited.
        if let Some(pool) = &req.agents_to_models {
            self.write_task_config(name, pool)?;
        }
        let updated = self
            .store
            .update(name, &req.to_store_update())?
            .ok_or_else(|| CommandError::Other(format!("task {name:?} was not found")))?;
        tracing::info!(
            task = %updated.name,
            interval_secs = updated.interval_secs,
            agent = ?updated.agent,
            model = ?updated.model,
            overlays = updated.overlays.len(),
            "squad task edited"
        );
        // An edit can add or drop an `env()` name; dropping the last one that
        // named a value is what garbage-collects it.
        self.refresh_required_env_best_effort().await;
        Ok(self.with_unmet_env_one(updated))
    }

    async fn list(&self) -> Result<Vec<Task>, CommandError> {
        // One store read serves both the refresh and the response, so the
        // indicator poller's single `list` per tick stays a single query.
        let tasks = self.store.list()?;
        self.refresh_required_env_from(&tasks).await;
        Ok(self.with_unmet_env(tasks))
    }

    async fn get(&self, name: &str) -> Result<Task, CommandError> {
        self.store
            .get(name)?
            .map(|task| self.with_unmet_env_one(task))
            .ok_or_else(|| CommandError::Other(format!("task {name:?} was not found")))
    }

    async fn runs(&self, name: &str, limit: usize) -> Result<Vec<Run>, CommandError> {
        // Existence is reported through the same not-found error shape the
        // other single-task methods use, so an unknown name never looks
        // like a task with no history.
        if self.store.get(name)?.is_none() {
            return Err(CommandError::Other(format!("task {name:?} was not found")));
        }
        Ok(self.store.runs_for(name, limit)?)
    }

    async fn set_status(&self, name: &str, status: TaskStatus) -> Result<(), CommandError> {
        if self.store.set_status(name, status)? {
            tracing::info!(
                task = %name,
                status = ?status,
                "squad administrator changed task status"
            );
            Ok(())
        } else {
            Err(CommandError::Other(format!("task {name:?} was not found")))
        }
    }

    /// Ask the scheduler to evaluate `name` on its next tick.
    ///
    /// A paused task is refused rather than silently ignored: pausing is an
    /// explicit "do not run this", and `due_for_evaluation` would drop the
    /// request on the floor, leaving the user watching a task that was
    /// triggered and never ran. Saying so — and naming `squad resume` — is the
    /// only honest answer.
    async fn trigger(&self, name: &str) -> Result<(), CommandError> {
        let task = self
            .store
            .get(name)?
            .ok_or_else(|| CommandError::Other(format!("task {name:?} was not found")))?;
        if task.status == TaskStatus::Paused {
            return Err(CommandError::Other(format!(
                "task {name:?} is paused and will not be evaluated; \
                 run `awman squad resume {name}` first"
            )));
        }
        if !self.store.request_trigger(name, crate::data::wire::now())? {
            return Err(CommandError::Other(format!("task {name:?} was not found")));
        }
        tracing::info!(task = %name, "squad administrator triggered task");
        Ok(())
    }

    /// Signal the scheduler to abandon `name`'s in-flight evaluation.
    ///
    /// Returns as soon as the signal is sent: the scheduler records the run as
    /// `canceled` and then stops the task's containers, which can take a few
    /// seconds per container and is not worth holding the request open for.
    async fn cancel(&self, name: &str) -> Result<(), CommandError> {
        let task = self
            .store
            .get(name)?
            .ok_or_else(|| CommandError::Other(format!("task {name:?} was not found")))?;
        let run_id = self
            .status
            .lock()
            .expect("scheduler status mutex poisoned")
            .cancel_run(task.id.as_str())
            .ok_or_else(|| CommandError::Other(format!("task {name:?} has no run in progress")))?;
        tracing::info!(task = %name, run_id = %run_id, "squad administrator canceled run");
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<(), CommandError> {
        let task = self.store.get(name)?;
        if self.store.delete(name)? {
            if let Some(task) = task {
                let mut status = self.status.lock().expect("scheduler status mutex");
                status.pending_runs.remove(task.id.as_str());
                if let Some(cancel) = status.cancellations.remove(task.id.as_str()) {
                    cancel.cancel();
                }
            }
            tracing::info!(task = %name, "squad administrator removed task");
            // Removing the last task that named a variable is the one
            // unambiguous signal that nothing needs its value any more, and
            // this is where that is noticed.
            self.refresh_required_env_best_effort().await;
            Ok(())
        } else {
            Err(CommandError::Other(format!("task {name:?} was not found")))
        }
    }

    async fn status(&self) -> Result<DaemonStatus, CommandError> {
        let tasks = self.store.list()?;
        self.refresh_required_env_from(&tasks).await;
        let env_persistence = Some(self.env_state.persistence());
        let unmet_env = self.env_state.unmet_names();
        let status = self.status.lock().expect("scheduler status mutex poisoned");
        Ok(DaemonStatus {
            running: true,
            pid: Some(std::process::id()),
            bound_addr: None,
            task_count: tasks.len(),
            active_count: tasks
                .iter()
                .filter(|c| c.status == TaskStatus::Active)
                .count(),
            last_tick: status.last_tick,
            in_flight: status.in_flight,
            config_error: status.config_error.clone(),
            env_persistence,
            unmet_env,
        })
    }

    async fn env_coverage(&self) -> Result<EnvCoverage, CommandError> {
        // Recomputed first, never cached, so the answer tracks tasks being
        // added and removed since the last command.
        self.refresh_required_env_best_effort().await;
        Ok(self.coverage())
    }

    async fn push_env(&self, push: EnvPush) -> Result<EnvPushResponse, CommandError> {
        self.refresh_required_env_best_effort().await;
        // `apply_push` writes the accepted values through to the keychain, a
        // call capped at five seconds. Doing that inline would park a runtime
        // worker for the whole cap on a locked keychain, once per push, which
        // an authenticated client can drive concurrently.
        let state = Arc::clone(&self.env_state);
        let now = crate::data::wire::now();
        let (accepted, ignored) =
            tokio::task::spawn_blocking(move || state.apply_push(push.vars, &push.absent, now))
                .await
                .map_err(|error| {
                    CommandError::Other(format!("squad env push task failed: {error}"))
                })?;
        if !accepted.is_empty() {
            // Names only. This line exists so an operator can see that a
            // rotation landed; a value must never reach it.
            tracing::info!(names = ?accepted, "squad env: accepted pushed values");
        }
        Ok(EnvPushResponse {
            accepted,
            ignored,
            coverage: self.coverage(),
        })
    }

    async fn clear_env_store(&self) -> Result<bool, CommandError> {
        // Unlike a failed store write this never degrades the daemon, so a
        // client could re-drive it indefinitely — five seconds of a parked
        // runtime worker per call if the keychain is wedged.
        let state = Arc::clone(&self.env_state);
        let cleared = tokio::task::spawn_blocking(move || state.clear_store())
            .await
            .map_err(|error| {
                CommandError::Other(format!("squad env clear task failed: {error}"))
            })?;
        Ok(cleared?)
    }

    async fn workflow_state(&self, name: &str) -> Result<Option<WorkflowState>, CommandError> {
        let Some(task) = self.store.get(name)? else {
            return Ok(None);
        };
        let Some(run) = self.store.running_run_for(&task.id)? else {
            return Ok(None);
        };
        let Some(path) = run.workflow_state_path else {
            return Ok(None);
        };
        Ok(crate::data::WorkflowStateStore::read_state_path(&path)?)
    }
}

fn task_custom_auth_env_names(
    env: &crate::data::config::env::EnvSnapshot,
    paths: &SquadPaths,
    global_squad: SquadConfig,
    task: &Task,
) -> Vec<String> {
    let pool = crate::data::config::squad::task_squad_config(paths, &task.name, &global_squad)
        .unwrap_or(global_squad);
    let mut agents: BTreeSet<String> = task.agent.iter().cloned().collect();
    agents.extend(pool.agents_to_models.unwrap_or_default().into_keys());
    let git_root = task.uses_worktree().then_some(task.repo_scope.as_path());
    let mut names: BTreeSet<String> = BTreeSet::new();
    for agent in agents {
        let Ok(agent) = crate::data::session::AgentName::new(&agent) else {
            continue;
        };
        if let Ok(resolved) = crate::engine::agent::AgentRegistry::resolve(env, &agent, git_root) {
            let def = resolved.definition();
            if def.is_custom() {
                names.extend(def.auth_env.iter().cloned());
            }
        }
    }
    names.into_iter().collect()
}

/// Every agent a repository-backed task may launch must resolve the way the
/// scheduler will resolve it (first-party, or an approved custom definition
/// in the repo or the daemon's global home) *and* have a Dockerfile, so a
/// deleted, invalid or unapproved definition is refused at create/update
/// rather than accepted and failed later. Never prompts: the daemon cannot.
fn validate_task_agents(
    env: &crate::data::config::env::EnvSnapshot,
    git_root: &std::path::Path,
    agents: &BTreeSet<String>,
) -> Result<(), CommandError> {
    let resolves = |agent: &str| -> Result<(), String> {
        let name = crate::data::session::AgentName::new(agent).map_err(|e| e.to_string())?;
        crate::engine::agent::AgentRegistry::resolve(env, &name, Some(git_root))
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    let unusable: Vec<(&String, String)> = agents
        .iter()
        .filter_map(|agent| resolves(agent).err().map(|e| (agent, e)))
        .collect();
    if !unusable.is_empty() {
        let mut message = String::from("squad task references agents that cannot be used:\n");
        for (agent, error) in unusable {
            message.push_str(&format!("  - \"{agent}\": {error}\n"));
        }
        return Err(CommandError::Other(message.trim_end().to_string()));
    }
    let paths = RepoDockerfilePaths::new(git_root);
    let unknown: Vec<_> = agents
        .iter()
        .filter(|agent| !paths.agent_dockerfile(agent).exists())
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    // A leftover Dockerfile whose definition is gone (or unapproved) is not
    // an available agent.
    let available = paths
        .discover_agent_dockerfiles()
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| resolves(name).is_ok())
        .collect::<Vec<_>>();
    let mut message =
        String::from("squad task references agents with no Dockerfile in the project:\n");
    for agent in unknown {
        message.push_str(&format!(
            "  - \"{agent}\" (expected .awman/Dockerfile.{agent})\n"
        ));
    }
    message.push_str(&format!(
        "Available agents: {}",
        if available.is_empty() {
            "(none)".to_string()
        } else {
            available.join(", ")
        }
    ));
    Err(CommandError::Other(message))
}

/// The env var names a task's workflow prerequisite policy allows a GitHub
/// token to come from (WI 0149). Empty unless the token use is `allow`.
fn prerequisite_token_env_names(task: &Task) -> Vec<String> {
    match &task.prerequisites {
        Some(policy)
            if policy.github_token.use_ == crate::data::config::workflows::CredentialUse::Allow =>
        {
            policy.github_token.env_vars.clone()
        }
        _ => Vec::new(),
    }
}

fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}
mod seconds_as_ms {
    pub fn serialize<S: serde::Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(v.saturating_mul(1000))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        use serde::Deserialize;
        let ms = u64::deserialize(d)?;
        if ms % 1000 != 0 {
            return Err(serde::de::Error::custom(
                "interval_ms must be a multiple of 1000 milliseconds (the scheduler interval has one-second granularity)",
            ));
        }
        Ok(ms / 1000)
    }
}
mod optional_seconds_as_ms {
    pub fn serialize<S: serde::Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        use serde::Serialize;
        v.map(|v| v.saturating_mul(1000)).serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        use serde::Deserialize;
        Option::<u64>::deserialize(d)?
            .map(|ms| {
                if ms % 1000 != 0 {
                    Err(serde::de::Error::custom(
                        "interval_ms must be a multiple of 1000 milliseconds (the scheduler interval has one-second granularity)",
                    ))
                } else {
                    Ok(ms / 1000)
                }
            })
            .transpose()
    }
}
