//! The effective config: built-in defaults → global → repo → task → env →
//! `--set`, merged per leaf.
//!
//! Precedence is encoded once, here:
//!
//! - an object at a non-leaf path recurses (per-leaf merge);
//! - a leaf (scalar, `null`, array, or an object at an `Object`/`StringMap`
//!   row) replaces the lower value, except the two additive rows
//!   (`overlays`, `workflows.dynamic.guidance`), which concatenate in
//!   precedence order and de-duplicate by trimmed value, first occurrence
//!   kept;
//! - a map keyed by user-chosen names (`workServers`, `remote.targets`,
//!   `agents.flags`, …) replaces as a whole when a file sets it;
//! - `null` from env or `--set` resets a leaf to its built-in default; `null`
//!   in a file means "not set here".
//!
//! Env and `--set` arrive as [`ConfigOverrides`] (see `overrides.rs`; parsed by
//! `env.rs`) and are applied leaf by leaf at their concrete path.
//!
//! Some policies are not a plain merge and keep reading the individual
//! layers: the builtin network policy (the trusted layers set a ceiling the
//! repo may only narrow) and workflow prerequisites (repo and task files may
//! narrow, never grant).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::data::config::agents::{AcpFallback, AgentAuthMode, LaunchMode};
use crate::data::config::builtin_launch::BuiltinLaunchSettings;
use crate::data::config::builtin_network::{BuiltinNetworkConfig, BuiltinNetworkSettings};
use crate::data::config::builtin_runtime::{BuiltinRuntimeConfig, DEFAULT_STATE_SUBDIR};
use crate::data::config::daemon::{DaemonSettings, Interaction};
use crate::data::config::env::EnvSnapshot;
use crate::data::config::fields::{self, ConfigKey, Merge, CONFIG_KEYS};
use crate::data::config::file::ConfigFile;
use crate::data::config::flags::FlagConfig;
use crate::data::config::overrides::ConfigOverrides;
use crate::data::config::remote::ResolvedRemoteTarget;
use crate::data::config::resolved_builtin::ResolvedBuiltinConfig;
use crate::data::config::runtime::{RuntimeSelection, UnknownRuntimeValue};
use crate::data::config::state::RepoState;
use crate::data::config::work_items::{
    WorkItemsConfig, DEFAULT_WORK_ITEMS_DIR, DEFAULT_WORK_ITEM_TEMPLATE,
};
use crate::data::config::work_servers::ResolvedWorkServer;
use crate::data::config::workflows::GUIDANCE_MAX_ENTRIES;
use crate::data::config::worktree::{OnDirtyMergeTarget, OnMergeFailure};
use crate::data::error::DataError;

/// The config sources, lowest precedence first (after the built-in
/// defaults, which come from the key table).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigLayers {
    pub global: ConfigFile,
    /// `Default` when not in a repo.
    pub repo: ConfigFile,
    /// A squad task's own file, during that task's evaluation only.
    pub task: Option<ConfigFile>,
    /// Derived `AWMAN_*` variables, resolved by `ConfigOverrides::from_env`.
    pub env: ConfigOverrides,
    /// `--set key=value`, resolved by `ConfigOverrides::parse_set_args`.
    pub set: ConfigOverrides,
}

impl ConfigLayers {
    /// Load the global file and, with a `git_root`, the repo file; derive the
    /// env layer from `env`; take `set` (`--set`) as given.
    pub fn load(
        env: &EnvSnapshot,
        git_root: Option<&Path>,
        set: ConfigOverrides,
    ) -> Result<Self, DataError> {
        use crate::data::config::file::{global_config_path, repo_config_path, FileKind};
        let global = ConfigFile::load(&global_config_path(env)?, FileKind::Global)?;
        let repo = match git_root {
            Some(root) => ConfigFile::load(&repo_config_path(root), FileKind::Repo)?,
            None => ConfigFile::default(),
        };
        let env_layer = env_overrides(env, &[&global, &repo])?;
        Ok(Self {
            global,
            repo,
            task: None,
            env: env_layer,
            set,
        })
    }

    /// The map keys present in these layers' files under `section` (a map
    /// path such as `workServers` or `remote.targets`), for splitting `*`
    /// segments of env names.
    pub fn map_names(&self, section: &str) -> Vec<String> {
        let mut files = vec![&self.global, &self.repo];
        files.extend(self.task.as_ref());
        map_names(&files, section)
    }
}

/// The map keys under `section` in `files`, de-duplicated, in order.
pub fn map_names(files: &[&ConfigFile], section: &str) -> Vec<String> {
    let segments: Vec<&str> = section.split('.').collect();
    let mut names: Vec<String> = fields::builtin_member_names(section)
        .iter()
        .map(|n| (*n).to_string())
        .collect();
    for file in files {
        let value = file.to_value();
        if let Some(object) = get_path(&value, &segments).and_then(Value::as_object) {
            for name in object.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
    }
    names
}

/// The env layer: every `AWMAN_*` config variable in `env`, resolved against
/// the key table by `ConfigOverrides::from_env` (`env.rs`). Map entries bind
/// to the names already present in `files`; a name only the environment uses
/// creates a new entry.
pub fn env_overrides(
    env: &EnvSnapshot,
    files: &[&ConfigFile],
) -> Result<ConfigOverrides, DataError> {
    ConfigOverrides::from_env(env, |section| map_names(files, section))
        .map_err(|e| DataError::Other(e.to_string()))
}

/// Where an effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Default,
    Global,
    Repo,
    Task,
    Env,
    Set,
    /// A per-invocation flag (`--leader`); not a config layer, but a source
    /// a resolved dynamic-workflow value can report.
    Flag,
}

impl Layer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Global => "global",
            Self::Repo => "repo",
            Self::Task => "task",
            Self::Env => "env",
            Self::Set => "set",
            Self::Flag => "flag",
        }
    }

    /// The file a layer is read from, for messages; `None` for non-file
    /// layers.
    pub fn file(self) -> Option<&'static str> {
        match self {
            Self::Global => Some("<AWMAN_HOME>/config.json"),
            Self::Repo => Some(".awman/config.json"),
            Self::Task => Some("the squad task config.json"),
            _ => None,
        }
    }

    /// Label a key and where the user set it: `workflows.dynamic.<field> in
    /// .awman/config.json`, `--leader`, `AWMAN_…`, `--set`.
    pub fn describe(self, field: &str) -> String {
        match self {
            Self::Flag => "--leader".into(),
            Self::Env => format!("workflows.dynamic.{field} (environment)"),
            Self::Set => format!("workflows.dynamic.{field} (--set)"),
            Self::Default => format!("workflows.dynamic.{field} (default)"),
            _ => format!(
                "workflows.dynamic.{field} in {}",
                self.file().unwrap_or_default()
            ),
        }
    }
}

/// The resolved dynamic-workflow source; one of the config [`Layer`]s or
/// the `--leader` flag.
pub type DynamicWorkflowsSource = Layer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Layer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceEntry {
    pub text: String,
    pub source: Layer,
}

/// Guidance entries per layer, after trimmed de-duplication.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuidanceCounts {
    pub global: usize,
    pub repo: usize,
    pub task: usize,
    pub overrides: usize,
}

/// `workflows.dynamic` resolved, with attribution retained for views and
/// diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDynamicWorkflows {
    pub default_leader: Option<Sourced<String>>,
    /// Always non-empty when set. A higher layer's map replaces the lower.
    pub agents_to_models: Option<Sourced<HashMap<String, Vec<String>>>>,
    pub guidance: Vec<GuidanceEntry>,
    pub guidance_counts: GuidanceCounts,
}

impl ResolvedDynamicWorkflows {
    pub fn guidance_texts(&self) -> Option<Vec<String>> {
        (!self.guidance.is_empty()).then(|| {
            self.guidance
                .iter()
                .map(|entry| entry.text.clone())
                .collect()
        })
    }
}

/// Resolved live credential-refresh settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthRefreshSettings {
    pub enabled: bool,
    pub threshold: Duration,
    pub tick: Duration,
}

/// The merged view of every config source, plus the invocation's flags and
/// environment snapshot. Access goes through methods so callers cannot
/// bypass the merge.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    layers: ConfigLayers,
    merged: ConfigFile,
    merged_value: Value,
    provenance: BTreeMap<String, Layer>,
    flags: FlagConfig,
    env: EnvSnapshot,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self::resolve(
            ConfigLayers::default(),
            FlagConfig::default(),
            EnvSnapshot::empty(),
        )
        .expect("the built-in defaults resolve")
    }
}

impl EffectiveConfig {
    /// Merge `layers` over the built-in defaults.
    ///
    /// Fails when the merged result is invalid (e.g. more than 50 guidance
    /// entries across layers, or an override value of the wrong type).
    pub fn resolve(
        layers: ConfigLayers,
        flags: FlagConfig,
        env: EnvSnapshot,
    ) -> Result<Self, DataError> {
        let (merged_value, provenance) = merge(&layers)?;
        // A name that differs from another only by `-` versus `_` would give
        // one env name two members; reject it across layers and overrides.
        fields::validate_map_names(&merged_value).map_err(|e| DataError::Other(e.to_string()))?;
        let merged: ConfigFile = serde_json::from_value(merged_value.clone())
            .map_err(|e| DataError::Other(format!("effective config is invalid: {e}")))?;
        // The network block is per-layer policy (ceiling + narrowing), so a
        // per-leaf merge of it is meaningless; each file validated its own.
        let mut check = merged.clone();
        if let Some(builtin) = check.runtime.as_mut().and_then(|r| r.builtin.as_mut()) {
            builtin.network = None;
        }
        check.validate_semantics()?;
        super::network::validate_network_config(
            &merged
                .daemon
                .clone()
                .unwrap_or_default()
                .network
                .unwrap_or_default(),
        )
        .map_err(DataError::Other)?;
        Ok(Self {
            layers,
            merged,
            merged_value,
            provenance,
            flags,
            env,
        })
    }

    /// Shorthand for global + repo files and no overrides.
    pub fn from_files(
        global: ConfigFile,
        repo: ConfigFile,
        flags: FlagConfig,
        env: EnvSnapshot,
    ) -> Result<Self, DataError> {
        Self::resolve(
            ConfigLayers {
                global,
                repo,
                ..Default::default()
            },
            flags,
            env,
        )
    }

    /// The same merge with different invocation flags.
    pub fn with_flags(mut self, flags: FlagConfig) -> Self {
        self.flags = flags;
        self
    }

    /// The same merge with a different environment snapshot (paths and
    /// process variables only; the env *layer* is part of [`ConfigLayers`]).
    pub fn with_env(mut self, env: EnvSnapshot) -> Self {
        self.env = env;
        self
    }

    /// The merged config, defaults applied.
    pub fn config(&self) -> &ConfigFile {
        &self.merged
    }

    /// The merged config as JSON (for `config show` / `GET /v1/config`).
    pub fn value(&self) -> &Value {
        &self.merged_value
    }

    /// The source layers.
    pub fn layers(&self) -> &ConfigLayers {
        &self.layers
    }

    /// Which layer set the effective value at a concrete path (a leaf, or a
    /// map entry). [`Layer::Default`] when no layer set it.
    pub fn source_of(&self, path: &str) -> Layer {
        let mut current = path;
        loop {
            if let Some(layer) = self.provenance.get(current) {
                return *layer;
            }
            match current.rsplit_once('.') {
                Some((parent, _)) => current = parent,
                None => return Layer::Default,
            }
        }
    }

    /// The merged JSON with every sensitive leaf masked (`••••` + last 4).
    pub fn masked_value(&self) -> Value {
        let mut value = self.merged_value.clone();
        mask_sensitive(&mut value, "", "");
        value
    }

    pub fn flags(&self) -> &FlagConfig {
        &self.flags
    }

    pub fn env(&self) -> &EnvSnapshot {
        &self.env
    }

    fn agents(&self) -> crate::data::config::agents::AgentsConfig {
        self.merged.agents.clone().unwrap_or_default()
    }

    /// The model override (flag only).
    pub fn model(&self) -> Option<String> {
        self.flags.model.clone()
    }

    /// The agent: `--agent` > `agents.default`.
    pub fn agent(&self) -> Option<String> {
        self.flags.agent.clone().or_else(|| self.agents().default)
    }

    /// `agents.yolo.disallowedTools`.
    pub fn yolo_disallowed_tools(&self) -> Vec<String> {
        self.agents()
            .yolo
            .and_then(|y| y.disallowed_tools)
            .unwrap_or_default()
    }

    /// Extra launch arguments for `agent`: `--agent-flag` when `agent` is
    /// the one this invocation selects, else `agents.flags.<agent>`.
    pub fn agent_flags(&self, agent: &str) -> Vec<String> {
        if let Some(values) = self.flags.agent_flags.as_ref() {
            if self.agent().as_deref() == Some(agent) {
                return values.clone();
            }
        }
        self.agents()
            .flags
            .and_then(|map| map.get(agent).cloned())
            .unwrap_or_default()
    }

    /// `agents.programStatus.enabled` (WI 0141): parse OSC 7501 reports and let
    /// them supersede silence detection. `false` keeps silence detection only.
    pub fn program_status_enabled(&self) -> bool {
        self.agents()
            .program_status
            .and_then(|p| p.enabled)
            .unwrap_or(true)
    }

    /// `tui.programStatus.emit` (WI 0141): emit OSC 7501 from the TUI to its
    /// own terminal when the terminal answers the feature probe.
    pub fn tui_program_status_emit(&self) -> bool {
        self.merged
            .tui
            .as_ref()
            .and_then(|t| t.program_status.as_ref())
            .and_then(|p| p.emit)
            .unwrap_or(true)
    }

    /// `tui.backend`, with the key-table default applied.
    pub fn tui_backend(&self) -> crate::data::config::tui::TuiBackend {
        self.merged
            .tui
            .as_ref()
            .and_then(|tui| tui.backend)
            .unwrap_or_default()
    }

    /// `tui.integrations.<name>.enabled`, resolved independently per
    /// integration and defaulting to automatic detection.
    pub fn integration_switch(&self, name: &str) -> crate::data::config::tui::IntegrationSwitch {
        self.merged
            .tui
            .as_ref()
            .map(|tui| tui.integration_switch(name))
            .unwrap_or_default()
    }

    /// `tui.scrollbackLines`.
    pub fn scrollback_lines(&self) -> usize {
        self.merged
            .tui
            .as_ref()
            .and_then(|t| t.scrollback_lines)
            .unwrap_or(10_000)
    }

    /// `agents.stuckTimeout`.
    pub fn agent_stuck_timeout(&self) -> Duration {
        self.agents()
            .stuck_timeout
            .map_or(Duration::from_secs(30), |d| d.to_duration())
    }

    /// `agents.launchMode`.
    pub fn launch_mode(&self) -> LaunchMode {
        self.agents().launch_mode.unwrap_or_default()
    }

    /// `agents.acpFallback`.
    pub fn acp_fallback(&self) -> AcpFallback {
        self.agents().acp_fallback.unwrap_or_default()
    }

    /// `agents.auth.mode`.
    pub fn auth_mode(&self) -> AgentAuthMode {
        self.agents().auth.and_then(|a| a.mode).unwrap_or_default()
    }

    /// `agents.auth.refresh.*`. `enabled: false` restores env-variable
    /// credential delivery.
    pub fn auth_refresh(&self) -> AuthRefreshSettings {
        let refresh = self
            .agents()
            .auth
            .and_then(|a| a.refresh)
            .unwrap_or_default();
        AuthRefreshSettings {
            enabled: refresh.enabled.unwrap_or(true),
            threshold: refresh
                .threshold
                .map_or(Duration::from_secs(20 * 60), |d| d.to_duration()),
            tick: refresh
                .tick
                .map_or(Duration::from_secs(60), |d| d.to_duration()),
        }
    }

    /// `--remote-session` > `AWMAN_REMOTE_SESSION`.
    pub fn remote_session(&self) -> Option<String> {
        self.flags
            .remote_session
            .clone()
            .or_else(|| self.env.remote_session().map(str::to_string))
    }

    /// `--headless` (flag only; daemon interaction defaults are
    /// `daemon.features.*.interaction`).
    pub fn headless(&self) -> bool {
        self.flags.headless.unwrap_or(false)
    }

    /// `runtime.kind`. Serde has already rejected an unknown spelling, so
    /// this is always `Ok`; the `Result` keeps the engine's error path.
    pub fn runtime_selection(&self) -> Result<RuntimeSelection, UnknownRuntimeValue> {
        Ok(self
            .merged
            .runtime
            .as_ref()
            .and_then(|r| r.kind)
            .unwrap_or_default())
    }

    /// `runtime.builtin` merged per leaf, defaults applied, without
    /// `network` (the enforced network policy is [`Self::builtin_network`]).
    pub fn builtin_runtime(&self) -> BuiltinRuntimeConfig {
        let merged = self
            .merged
            .runtime
            .as_ref()
            .and_then(|r| r.builtin.clone())
            .unwrap_or_default();
        BuiltinRuntimeConfig {
            network: None,
            ..merged
        }
    }

    /// Every builtin-runtime setting the engine boundary needs, validated.
    pub fn builtin_settings(&self) -> Result<ResolvedBuiltinConfig, DataError> {
        let builtin = self.builtin_runtime();
        builtin
            .validate_image_sources()
            .map_err(DataError::BuiltinConfig)?;
        let launch = self.builtin_launch_settings()?;
        let builder_enabled = builtin.builder_enabled();
        let process = self.builtin_process_settings()?;
        Ok(ResolvedBuiltinConfig {
            state_dir: process.state_dir,
            ambient_overrides: process.ambient_overrides,
            test_isolation: process.test_isolation,
            launch,
            image_source: builtin.image_source,
            images: builtin.images,
            registries: builtin.registries,
            builder_enabled,
        })
    }

    /// The builtin network policy: the trusted layers (global file, env,
    /// `--set`) set the ceiling and the repo block may only narrow it.
    pub fn builtin_network(&self) -> Result<BuiltinNetworkSettings, String> {
        let network = |file: &ConfigFile| -> Option<BuiltinNetworkConfig> {
            file.runtime
                .as_ref()
                .and_then(|r| r.builtin.as_ref())
                .and_then(|b| b.network.clone())
        };
        let ceiling = self.trusted_network_value();
        let ceiling: Option<BuiltinNetworkConfig> = match ceiling {
            Some(value) => Some(
                serde_json::from_value(value)
                    .map_err(|e| format!("runtime.builtin.network: {e}"))?,
            ),
            None => None,
        };
        BuiltinNetworkConfig::resolve(ceiling.as_ref(), network(&self.layers.repo).as_ref())
    }

    /// The global file's `runtime.builtin.network` with env and `--set`
    /// network leaves applied over it.
    fn trusted_network_value(&self) -> Option<Value> {
        const PREFIX: &str = "runtime.builtin.network.";
        let mut value = self
            .layers
            .global
            .runtime
            .as_ref()
            .and_then(|r| r.builtin.as_ref())
            .and_then(|b| b.network.as_ref())
            .map(|n| serde_json::to_value(n).expect("network serializes"));
        for (path, v) in self.layers.env.iter().chain(self.layers.set.iter()) {
            if let Some(leaf) = path.strip_prefix(PREFIX) {
                let object = value.get_or_insert_with(|| Value::Object(Default::default()));
                if v.is_null() {
                    object.as_object_mut().map(|o| o.remove(leaf));
                } else {
                    object[leaf] = v.clone();
                }
            }
        }
        value
    }

    /// The sizing and network policy of one builtin launch for this session.
    pub fn builtin_launch_settings(&self) -> Result<BuiltinLaunchSettings, DataError> {
        let builtin = self.builtin_runtime();
        builtin
            .validate_resources()
            .map_err(DataError::BuiltinConfig)?;
        let network = self.builtin_network().map_err(DataError::BuiltinConfig)?;
        let repo_sets_state_dir = self
            .layers
            .repo
            .runtime
            .as_ref()
            .and_then(|r| r.builtin.as_ref())
            .is_some_and(|b| b.state_dir.is_some());
        let repo_state_dir = if repo_sets_state_dir {
            Some(self.builtin_state_dir()?)
        } else {
            None
        };
        Ok(BuiltinLaunchSettings {
            vcpus: builtin.vcpus_or_default(),
            memory_mib: builtin.memory_mib_or_default(),
            root_disk_mib: builtin.root_disk_mib_or_default(),
            mount_quota_mib: builtin.mount_quota_mib_or_default(),
            network,
            repo_state_dir,
        })
    }

    /// The process-wide builtin settings runtime detection needs.
    pub fn builtin_process_settings(
        &self,
    ) -> Result<crate::data::config::resolved_builtin::BuiltinProcessConfig, DataError> {
        Ok(
            crate::data::config::resolved_builtin::BuiltinProcessConfig {
                state_dir: self.builtin_state_dir()?,
                ambient_overrides: self.env.ambient_msb_overrides(),
                test_isolation: crate::data::config::env::test_isolation_active()
                    && !self.env.test_builtin(),
            },
        )
    }

    /// `runtime.builtin.stateDir`, else `<data home>/builtin`.
    pub fn builtin_state_dir(&self) -> Result<PathBuf, DataError> {
        if let Some(dir) = self.builtin_runtime().state_dir {
            return Ok(dir);
        }
        Ok(crate::data::fs::data_paths::data_home(&self.env)?.join(DEFAULT_STATE_SUBDIR))
    }

    /// `runtime.baseImage`.
    pub fn base_image(&self) -> Option<String> {
        self.merged
            .runtime
            .as_ref()
            .and_then(|r| r.base_image.clone())
    }

    /// `runtime.dockerfile` resolved against `git_root`.
    pub fn dockerfile_path(&self, git_root: &Path) -> PathBuf {
        let dockerfile = self
            .merged
            .runtime
            .as_ref()
            .and_then(|r| r.dockerfile.clone())
            .unwrap_or_else(|| "Dockerfile.dev".into());
        resolve_under(git_root, &dockerfile)
    }

    /// `workItems.dir` resolved against `git_root`.
    pub fn work_items_dir(&self, git_root: &Path) -> PathBuf {
        let dir = self
            .merged
            .work_items
            .as_ref()
            .and_then(|w| w.dir.clone())
            .unwrap_or_else(|| DEFAULT_WORK_ITEMS_DIR.into());
        resolve_under(git_root, &dir)
    }

    /// `workItems.template` resolved against `git_root`, else
    /// `<work items dir>/0000-template.md`.
    pub fn work_items_template(&self, git_root: &Path) -> PathBuf {
        match &self.merged.work_items {
            Some(WorkItemsConfig {
                template: Some(template),
                ..
            }) => resolve_under(git_root, template),
            _ => self
                .work_items_dir(git_root)
                .join(DEFAULT_WORK_ITEM_TEMPLATE),
        }
    }

    /// `workflows.maxConcurrentSteps` (`None` = unlimited).
    pub fn max_concurrent_steps(&self) -> Option<usize> {
        self.merged
            .workflows
            .as_ref()
            .and_then(|w| w.max_concurrent_steps)
    }

    /// `worktree.onDirtyMergeTarget`, when a layer sets it. `None` otherwise,
    /// so the frontend class default (interactive: ask; headless: its own)
    /// applies.
    pub fn worktree_on_dirty_merge_target(&self) -> Option<OnDirtyMergeTarget> {
        if self.source_of("worktree.onDirtyMergeTarget") == Layer::Default {
            return None;
        }
        self.merged
            .worktree
            .as_ref()
            .and_then(|w| w.on_dirty_merge_target)
    }

    /// `worktree.mergeFix.onFailure`, when a layer sets it (see above).
    pub fn worktree_merge_fix_on_failure(&self) -> Option<OnMergeFailure> {
        if self.source_of("worktree.mergeFix.onFailure") == Layer::Default {
            return None;
        }
        self.merged
            .worktree
            .as_ref()
            .and_then(|w| w.merge_fix.as_ref())
            .and_then(|m| m.on_failure)
    }

    /// `worktree.mergeFix.promptTemplate`, relative to the git root.
    pub fn worktree_merge_fix_prompt_template(&self) -> Option<String> {
        self.merged
            .worktree
            .as_ref()
            .and_then(|w| w.merge_fix.as_ref())
            .and_then(|m| m.prompt_template.clone())
    }

    /// Every `daemon` leaf with defaults applied.
    pub fn daemon(&self) -> DaemonSettings {
        let mut settings =
            DaemonSettings::from_merged(&self.merged.daemon.clone().unwrap_or_default());
        settings.worker.work_servers = self.work_servers();
        settings
    }

    /// Work servers present in the effective config, with built-in defaults
    /// applied to `enabled`, `claim` and `maxConcurrentJobs`.
    pub fn work_servers(&self) -> BTreeMap<String, ResolvedWorkServer> {
        self.merged
            .work_servers
            .as_ref()
            .into_iter()
            .flat_map(|servers| servers.iter())
            .map(|(name, config)| (name.clone(), config.resolved()))
            .collect()
    }

    /// Whether this TUI invocation is configured to host a worker.
    pub fn tui_worker_enabled(&self) -> bool {
        self.merged
            .tui
            .as_ref()
            .and_then(|tui| tui.worker.as_ref())
            .and_then(|worker| worker.enabled)
            .unwrap_or(false)
    }

    /// Named network listeners with built-in defaults applied.
    pub fn network_listeners(
        &self,
    ) -> std::collections::BTreeMap<String, crate::data::config::daemon::ListenerSettings> {
        self.daemon().network.listeners
    }

    /// `daemon.features.worker.push.workDirs.allow`.
    pub fn worker_push_work_dirs(&self) -> Vec<String> {
        self.daemon().worker.push_work_dirs
    }

    /// `daemon.features.worker.push.workDirs.roots`.
    pub fn worker_push_work_dir_roots(&self) -> Vec<String> {
        self.daemon().worker.push_work_dir_roots
    }

    /// `daemon.features.worker.push.interaction` and `.allowOverride`.
    pub fn worker_push_interaction(&self) -> (Interaction, bool) {
        let worker = self.daemon().worker;
        (worker.push_interaction, worker.push_allow_override)
    }

    /// `name`, else `remote.default`, resolved to a configured target.
    pub fn remote_target(&self, name: Option<&str>) -> Result<ResolvedRemoteTarget, DataError> {
        self.merged
            .remote
            .clone()
            .unwrap_or_default()
            .resolve_target(name)
    }

    /// Per-repo state (not config) for `git_root`.
    pub fn repo_state(&self, git_root: &Path) -> RepoState {
        crate::data::fs::DataPaths::from_env(&self.env)
            .and_then(|data| RepoState::load(&data, git_root))
            .unwrap_or_default()
    }

    /// Workflow prerequisite policy: global (with env and `--set` leaves,
    /// the trusted layers) → repo (narrow only) → task file (narrow only) →
    /// a stored task policy → flags.
    pub fn prerequisite_policy(
        &self,
        context: super::workflows::PrerequisiteResolveContext,
    ) -> super::workflows::PrerequisitePolicy {
        use super::workflows::{PrerequisitePolicy, PrerequisitePolicyLayers, PrerequisitesConfig};
        let prerequisites = |file: &ConfigFile| -> Option<PrerequisitesConfig> {
            file.workflows
                .as_ref()
                .and_then(|w| w.prerequisites.clone())
        };
        let trusted = self.trusted_prerequisites();
        let repo = prerequisites(&self.layers.repo);
        let task = self.layers.task.as_ref().and_then(prerequisites);
        PrerequisitePolicy::resolve(PrerequisitePolicyLayers {
            global: trusted.as_ref(),
            task_file: task.as_ref(),
            repo: repo.as_ref(),
            task: self.flags.squad_task_prerequisites.as_ref(),
            flags: &self.flags.prerequisites,
            context,
        })
    }

    /// The global file's `workflows.prerequisites` with env and `--set`
    /// prerequisite leaves applied over it.
    fn trusted_prerequisites(&self) -> Option<super::workflows::PrerequisitesConfig> {
        const PREFIX: &str = "workflows.prerequisites.";
        let mut value = self
            .layers
            .global
            .workflows
            .as_ref()
            .and_then(|w| w.prerequisites.as_ref())
            .map(|p| serde_json::to_value(p).expect("prerequisites serialize"));
        for (path, v) in self.layers.env.iter().chain(self.layers.set.iter()) {
            if let Some(leaf) = path.strip_prefix(PREFIX) {
                let root = value.get_or_insert_with(|| Value::Object(Default::default()));
                set_path(root, &leaf.split('.').collect::<Vec<_>>(), v.clone());
            }
        }
        value.and_then(|v| serde_json::from_value(v).ok())
    }

    /// `workflows.dynamic`, attributed: `--leader` > the layer that set
    /// `defaultLeader`; the map from the highest layer that set it; guidance
    /// from every layer in precedence order, de-duplicated.
    pub fn dynamic_workflows(&self) -> Result<ResolvedDynamicWorkflows, DataError> {
        let dynamic = self
            .merged
            .workflows
            .as_ref()
            .and_then(|w| w.dynamic.clone())
            .unwrap_or_default();
        let default_leader = match self.flags.leader.clone() {
            Some(value) => Some(Sourced {
                value,
                source: Layer::Flag,
            }),
            None => dynamic.default_leader.map(|value| Sourced {
                value,
                source: self.source_of("workflows.dynamic.defaultLeader"),
            }),
        };
        let agents_to_models = dynamic
            .agents_to_models
            .filter(|map| !map.is_empty())
            .map(|map| Sourced {
                source: self.source_of("workflows.dynamic.agentsToModels"),
                value: map.into_iter().collect(),
            });
        // Re-attribute each merged guidance entry to the first layer that
        // contributed it.
        let mut sources: Vec<(String, Layer)> = Vec::new();
        let layer_guidance = |file: &ConfigFile| -> Vec<String> {
            file.workflows
                .as_ref()
                .and_then(|w| w.dynamic.as_ref())
                .and_then(|d| d.guidance.clone())
                .unwrap_or_default()
        };
        for (layer, entries) in [
            (Layer::Global, layer_guidance(&self.layers.global)),
            (Layer::Repo, layer_guidance(&self.layers.repo)),
            (
                Layer::Task,
                self.layers
                    .task
                    .as_ref()
                    .map(layer_guidance)
                    .unwrap_or_default(),
            ),
        ] {
            sources.extend(entries.into_iter().map(|e| (e.trim().to_string(), layer)));
        }
        let mut counts = GuidanceCounts::default();
        let guidance: Vec<GuidanceEntry> = dynamic
            .guidance
            .unwrap_or_default()
            .into_iter()
            .map(|text| {
                let source = sources
                    .iter()
                    .find(|(t, _)| *t == text.trim())
                    .map_or(Layer::Set, |(_, l)| *l);
                match source {
                    Layer::Global => counts.global += 1,
                    Layer::Repo => counts.repo += 1,
                    Layer::Task => counts.task += 1,
                    _ => counts.overrides += 1,
                }
                GuidanceEntry { text, source }
            })
            .collect();
        Ok(ResolvedDynamicWorkflows {
            default_leader,
            agents_to_models,
            guidance,
            guidance_counts: counts,
        })
    }
}

fn resolve_under(git_root: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        git_root.join(p)
    }
}

// ---------------------------------------------------------------------------
// The merge
// ---------------------------------------------------------------------------

/// The built-in defaults as one document: every row's default at its path;
/// `*` rows contribute only an empty map at their map's path.
pub fn default_document() -> Value {
    let mut doc = Value::Object(Default::default());
    for key in CONFIG_KEYS {
        let segments: Vec<&str> = key.path.split('.').collect();
        match segments.iter().position(|s| *s == "*") {
            Some(star) => {
                if get_path(&doc, &segments[..star]).is_none() {
                    set_path(
                        &mut doc,
                        &segments[..star],
                        Value::Object(Default::default()),
                    );
                }
            }
            None => set_path(&mut doc, &segments, key.default_value()),
        }
    }
    for name in ["default", "loopback"] {
        for leaf in [
            "enabled",
            "listen",
            "transport",
            "features",
            "tlsSans",
            "allowedOrigins",
        ] {
            set_path(
                &mut doc,
                &["daemon", "network", "listeners", name, leaf],
                super::network::builtin_leaf(name, leaf).unwrap(),
            );
        }
    }
    doc
}

fn get_path<'a>(doc: &'a Value, segments: &[&str]) -> Option<&'a Value> {
    segments.iter().try_fold(doc, |v, s| v.get(*s))
}

fn set_path(doc: &mut Value, segments: &[&str], value: Value) {
    let mut current = doc;
    for (i, segment) in segments.iter().enumerate() {
        if !current.is_object() {
            *current = Value::Object(Default::default());
        }
        let object = current.as_object_mut().expect("just made an object");
        if i + 1 == segments.len() {
            object.insert((*segment).to_string(), value);
            return;
        }
        current = object
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Default::default()));
    }
}

fn row_for(pattern: &str) -> Option<&'static ConfigKey> {
    CONFIG_KEYS.iter().find(|k| k.path == pattern)
}

fn is_map_level(pattern: &str) -> bool {
    let star = if pattern.is_empty() {
        "*".to_string()
    } else {
        format!("{pattern}.*")
    };
    CONFIG_KEYS
        .iter()
        .any(|k| k.path == star || k.path.starts_with(&format!("{star}.")))
}

/// Named maps whose entries are resolved independently rather than replaced
/// as a whole. A repo that configures one TUI integration must not silently
/// reset every other integration the global file configured, so each
/// `tui.integrations.<name>` block merges by name, and inside a block each
/// top-level setting (`enabled` and the integration's own keys) takes the
/// highest layer that sets it.
const PER_ENTRY_MAPS: &[&str] = &["tui.integrations"];

/// Merge one layer's [`PER_ENTRY_MAPS`] map into `target`, entry by entry and
/// within an entry setting by setting, recording provenance per setting.
fn merge_per_entry(
    target: &mut Value,
    path: &str,
    incoming: &Value,
    layer: Layer,
    provenance: &mut BTreeMap<String, Layer>,
) {
    let Some(entries) = incoming.as_object() else {
        return;
    };
    for (name, entry) in entries {
        if entry.is_null() {
            continue;
        }
        let entry_path = child(path, name);
        let Some(settings) = entry.as_object() else {
            // Not an object: the key-table walker rejects this in a file, so
            // keep the old whole-value behaviour rather than guess.
            let segments: Vec<&str> = entry_path.split('.').collect();
            set_path(target, &segments, entry.clone());
            provenance.insert(entry_path, layer);
            continue;
        };
        if settings.is_empty() {
            // An empty block still declares the integration.
            let segments: Vec<&str> = entry_path.split('.').collect();
            if get_path(target, &segments).is_none() {
                set_path(target, &segments, entry.clone());
            }
            continue;
        }
        for (setting, value) in settings {
            if value.is_null() {
                continue;
            }
            let setting_path = child(&entry_path, setting);
            let segments: Vec<&str> = setting_path.split('.').collect();
            set_path(target, &segments, value.clone());
            let prefix = format!("{setting_path}.");
            provenance.retain(|p, _| !p.starts_with(&prefix));
            provenance.insert(setting_path, layer);
        }
    }
}

fn child(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}.{segment}")
    }
}

/// Merge every layer over the defaults; returns the document and, per
/// concrete path, the layer that set it.
fn merge(layers: &ConfigLayers) -> Result<(Value, BTreeMap<String, Layer>), DataError> {
    let mut doc = default_document();
    let mut provenance = BTreeMap::new();
    let files = [
        (Layer::Global, Some(&layers.global)),
        (Layer::Repo, Some(&layers.repo)),
        (Layer::Task, layers.task.as_ref()),
    ];
    for (layer, file) in files {
        if let Some(file) = file {
            merge_file(&mut doc, &file.to_value(), "", "", layer, &mut provenance);
        }
    }
    for (layer, overrides) in [(Layer::Env, &layers.env), (Layer::Set, &layers.set)] {
        for (path, value) in overrides.iter() {
            apply_override(&mut doc, path, value.clone(), layer, &mut provenance)?;
        }
    }
    fill_map_defaults(&mut doc, &mut provenance);
    let guidance = get_path(&doc, &["workflows", "dynamic", "guidance"])
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if guidance > GUIDANCE_MAX_ENTRIES {
        return Err(DataError::Other(format!(
            "workflows.dynamic.guidance has {guidance} entries across the global, repo and task \
             config files and overrides; the limit is {GUIDANCE_MAX_ENTRIES}"
        )));
    }
    Ok((doc, provenance))
}

/// Give every existing named-map entry the table defaults of its leaves
/// (`workServers.prod.enabled` → `true`), reported with default provenance.
/// A wildcard row never invents an entry; it only completes existing ones.
fn fill_map_defaults(doc: &mut Value, provenance: &mut BTreeMap<String, Layer>) {
    for key in CONFIG_KEYS {
        let Some((map_path, leaf)) = key.path.split_once(".*.") else {
            continue;
        };
        let default = key.default_value();
        if leaf.contains('*') || map_path.contains('*') || default.is_null() {
            continue;
        }
        let map_segments: Vec<&str> = map_path.split('.').collect();
        let leaf_segments: Vec<&str> = leaf.split('.').collect();
        let Some(names) = get_path(doc, &map_segments)
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect::<Vec<_>>())
        else {
            continue;
        };
        for name in names {
            let mut full: Vec<&str> = map_segments.clone();
            full.push(&name);
            full.extend(&leaf_segments);
            if get_path(doc, &full).is_some_and(|v| !v.is_null()) {
                continue;
            }
            if get_path(doc, &full[..map_segments.len() + 1]).is_some_and(Value::is_object) {
                let value = if map_path == "daemon.network.listeners" {
                    super::network::builtin_leaf(&name, leaf).unwrap_or_else(|| default.clone())
                } else {
                    default.clone()
                };
                set_path(doc, &full, value);
                provenance.insert(full.join("."), Layer::Default);
            }
        }
    }
}

fn merge_file(
    target: &mut Value,
    incoming: &Value,
    concrete: &str,
    pattern: &str,
    layer: Layer,
    provenance: &mut BTreeMap<String, Layer>,
) {
    let Some(object) = incoming.as_object() else {
        return;
    };
    for (name, value) in object {
        if value.is_null() {
            // `null` in a file means "not set here".
            continue;
        }
        let path = child(concrete, name);
        let child_pattern = child(pattern, name);
        if let Some(key) = row_for(&child_pattern) {
            set_leaf(target, &path, key, value.clone(), layer, provenance);
        } else if PER_ENTRY_MAPS.contains(&child_pattern.as_str()) {
            merge_per_entry(target, &path, value, layer, provenance);
        } else if is_map_level(&child_pattern) {
            // A map keyed by user-chosen names replaces as a whole.
            let segments: Vec<&str> = path.split('.').collect();
            set_path(target, &segments, value.clone());
            let prefix = format!("{path}.");
            provenance.retain(|p, _| !p.starts_with(&prefix));
            provenance.insert(path.clone(), layer);
        } else {
            merge_file(target, value, &path, &child_pattern, layer, provenance);
        }
    }
}

/// Set one leaf (by concrete path) with the leaf rule: replace, or
/// concatenate and de-duplicate for additive rows.
fn set_leaf(
    target: &mut Value,
    path: &str,
    key: &ConfigKey,
    value: Value,
    layer: Layer,
    provenance: &mut BTreeMap<String, Layer>,
) {
    let segments: Vec<&str> = path.split('.').collect();
    if value.is_null() {
        let default = if segments.len() == 5 && segments[..3] == ["daemon", "network", "listeners"]
        {
            super::network::builtin_leaf(segments[3], segments[4])
                .unwrap_or_else(|| key.default_value())
        } else {
            key.default_value()
        };
        set_path(target, &segments, default);
        provenance.remove(path);
        return;
    }
    let value = if key.merge == Merge::Concat {
        let mut items: Vec<Value> = get_path(target, &segments)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        items.extend(value.as_array().cloned().unwrap_or_default());
        let mut seen = HashSet::new();
        items.retain(|v| seen.insert(v.as_str().map(str::trim).unwrap_or_default().to_string()));
        Value::Array(items)
    } else {
        value
    };
    set_path(target, &segments, value);
    provenance.insert(path.to_string(), layer);
}

fn apply_override(
    target: &mut Value,
    path: &str,
    value: Value,
    layer: Layer,
    provenance: &mut BTreeMap<String, Layer>,
) -> Result<(), DataError> {
    let resolved = fields::resolve(path).map_err(|e| DataError::Other(e.to_string()))?;
    fields::validate_value(resolved.key, &value).map_err(|e| DataError::Other(e.to_string()))?;
    set_leaf(target, path, resolved.key, value, layer, provenance);
    Ok(())
}

fn mask_sensitive(value: &mut Value, concrete: &str, pattern: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let map_level = is_map_level(pattern);
    for (name, child_value) in object.iter_mut() {
        let path = child(concrete, name);
        let child_pattern = if map_level {
            child(pattern, "*")
        } else {
            child(pattern, name)
        };
        if let Some(key) = row_for(&child_pattern) {
            if key.sensitive {
                if let Some(s) = child_value.as_str() {
                    *child_value = Value::String(mask(s));
                }
            }
        } else {
            mask_sensitive(child_value, &path, &child_pattern);
        }
    }
}

/// `••••` plus the last four characters (all bullets for short values).
pub fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "••••".into();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}
