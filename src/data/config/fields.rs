//! The key table: the single source of truth for every leaf config key.
//!
//! One row per leaf of the config tree. File validation (unknown keys, scope,
//! value shape), the built-in defaults layer, merge behaviour, `config show`
//! data, masking, live-reload classification, derived environment-variable
//! names and the generated docs table all come from [`CONFIG_KEYS`].
//!
//! Layer 0: the rows *are* the config schema.

use std::collections::BTreeMap;

use serde_json::Value;

/// Which files a key may appear in. `Task` includes repo and global; `Repo`
/// includes global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyScope {
    Global,
    Repo,
    Task,
}

impl KeyScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Repo => "repo",
            Self::Task => "task",
        }
    }

    /// The files a key of this scope may appear in, for error messages.
    pub fn allowed_files(self) -> &'static str {
        match self {
            Self::Global => "the global config file (<AWMAN_HOME>/config.json)",
            Self::Repo => {
                "the global or repo config file (<AWMAN_HOME>/config.json, <repo>/.awman/config.json)"
            }
            Self::Task => {
                "the global, repo or squad task config file (<AWMAN_HOME>/config.json, \
                 <repo>/.awman/config.json, <AWMAN_HOME>/squad/tasks/<name>/config.json)"
            }
        }
    }
}

/// The kind of file a key was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Global,
    Repo,
    Task,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "the global config file",
            Self::Repo => "a repo config file",
            Self::Task => "a squad task config file",
        }
    }

    /// Whether a key of `scope` may appear in a file of this kind.
    pub fn admits(self, scope: KeyScope) -> bool {
        match self {
            Self::Global => true,
            Self::Repo => scope >= KeyScope::Repo,
            Self::Task => scope == KeyScope::Task,
        }
    }
}

/// What a leaf value is. Drives parsing of `--set`/env/`config set` input,
/// validation of file values, and the docs "type" column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Bool,
    /// Inclusive bounds; `None` = unbounded on that side.
    Int {
        min: Option<i64>,
        max: Option<i64>,
    },
    String,
    /// Closed set of lowercase / snake_case spellings.
    Enum(&'static [&'static str]),
    /// `"500ms" | "30s" | "15m" | "2h"`.
    Duration,
    /// `"512MiB" | "4GiB"`. Binary units only.
    Size,
    /// Comma-separated on `--set`/env/`config set`; a JSON array in files.
    StringList,
    IntList,
    /// `k=v,k2=v2` on scalar input; a JSON object of strings in files.
    StringMap,
    /// Opaque typed JSON (`ImageSourceSpec`, `RegistryHostConfig`, …): shown
    /// as a summary, never settable through `--set`/env/`config set`.
    Object,
}

impl ValueKind {
    /// The docs "type" column.
    pub fn describe(&self) -> String {
        match self {
            Self::Bool => "bool".into(),
            Self::Int { min, max } => match (min, max) {
                (Some(lo), Some(hi)) => format!("int {lo}..{hi}"),
                (Some(lo), None) => format!("int ≥ {lo}"),
                (None, Some(hi)) => format!("int ≤ {hi}"),
                (None, None) => "int".into(),
            },
            Self::String => "string".into(),
            Self::Enum(values) => format!("enum `{}`", values.join("\\|")),
            Self::Duration => "duration".into(),
            Self::Size => "size".into(),
            Self::StringList => "list<string>".into(),
            Self::IntList => "list<int>".into(),
            Self::StringMap => "map<string,string>".into(),
            Self::Object => "object".into(),
        }
    }
}

/// How a key combines across layers. Scalars always replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    Replace,
    /// Arrays only: concatenate in precedence order, dedupe by trimmed value.
    Concat,
}

/// One leaf key. `path` segments are `camelCase`; a `*` segment is a
/// user-chosen name (`workServers.*.url`), validated by [`MAP_NAME`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigKey {
    pub path: &'static str,
    pub kind: ValueKind,
    /// JSON literal text of the built-in default (`"null"`, `"\"30s\""`,
    /// `"[]"`, `"{}"`, `"2"`). Parsed by [`ConfigKey::default_value`].
    pub default: &'static str,
    pub scope: KeyScope,
    /// Masked in every output (`config show/get`, `GET /v1/config`, logs).
    pub sensitive: bool,
    /// A running daemon picks it up on `POST /v1/config/reload`.
    pub live: bool,
    pub merge: Merge,
    /// One sentence; the docs "meaning" column.
    pub doc: &'static str,
}

/// `[a-z0-9][a-z0-9_-]{0,62}` — the only shape a `*` segment may take.
pub const MAP_NAME: &str = "[a-z0-9][a-z0-9_-]{0,62}";

/// Process variables the derived names may never collide with.
pub const RESERVED_ENV: &[&str] = &[
    "AWMAN_HOME",
    "AWMAN_REMOTE_SESSION",
    "AWMAN_LEASE_CREDENTIAL_FILE",
    "AWMAN_TEST_ISOLATION",
    "AWMAN_TEST_DOCKER",
    "AWMAN_TEST_APPLE_CONTAINER",
    "AWMAN_TEST_SBX",
    "AWMAN_TEST_BUILTIN",
    "AWMAN_TEST_YOLO_COUNTDOWN_MS",
    "AWMAN_TEST_AGENT_DONE_COUNTDOWN_MS",
];

/// Whether `name` matches [`MAP_NAME`].
pub fn is_valid_map_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let ok_first = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let ok_rest = |b: u8| ok_first(b) || b == b'_' || b == b'-';
    !bytes.is_empty()
        && bytes.len() <= 63
        && ok_first(bytes[0])
        && bytes[1..].iter().all(|b| ok_rest(*b))
}

/// The duplicate-detection form of a map name: `-` folded to `_`.
pub fn fold_map_name(name: &str) -> String {
    name.replace('-', "_")
}

const fn row(
    path: &'static str,
    kind: ValueKind,
    default: &'static str,
    scope: KeyScope,
    live: bool,
    doc: &'static str,
) -> ConfigKey {
    ConfigKey {
        path,
        kind,
        default,
        scope,
        sensitive: false,
        live,
        merge: Merge::Replace,
        doc,
    }
}

const fn int(min: Option<i64>, max: Option<i64>) -> ValueKind {
    ValueKind::Int { min, max }
}

const POSITIVE: ValueKind = int(Some(1), None);
const CREDENTIAL_USE: ValueKind = ValueKind::Enum(&["ask", "allow", "deny"]);
const INTERACTION: ValueKind = ValueKind::Enum(&["interactive", "headless"]);

use KeyScope::{Global, Repo, Task};
use ValueKind::{Bool, Duration, Object, Size, StringList, StringMap};

/// Every leaf, in `config show` display order.
pub const CONFIG_KEYS: &[ConfigKey] = &[
    // agents
    row(
        "agents.default",
        ValueKind::String,
        "null",
        Task,
        false,
        "Agent used when no --agent flag names one.",
    ),
    row(
        "agents.stuckTimeout",
        Duration,
        "\"30s\"",
        Task,
        false,
        "Inactivity after which an agent is reported as stuck.",
    ),
    row(
        "agents.launchMode",
        ValueKind::Enum(&["stdio", "acp"]),
        "\"stdio\"",
        Task,
        false,
        "How an agent process is connected to awman.",
    ),
    row(
        "agents.acpFallback",
        ValueKind::Enum(&["stdio", "error"]),
        "\"error\"",
        Repo,
        false,
        "What to do when ACP is requested for an agent that does not support it.",
    ),
    row(
        "agents.flags.*",
        StringList,
        "[]",
        Task,
        false,
        "Extra arguments appended to the named agent's launch command.",
    ),
    row(
        "agents.yolo.disallowedTools",
        StringList,
        "[]",
        Task,
        false,
        "Tools an agent may not use in yolo mode.",
    ),
    row(
        "agents.auth.mode",
        ValueKind::Enum(&["keychain", "passthrough", "none"]),
        "\"keychain\"",
        Repo,
        false,
        "How host credentials are injected into agent containers.",
    ),
    row(
        "agents.auth.refresh.enabled",
        Bool,
        "true",
        Repo,
        false,
        "Refresh agent credentials while an agent runs.",
    ),
    row(
        "agents.auth.refresh.threshold",
        Duration,
        "\"20m\"",
        Repo,
        false,
        "Refresh a credential when it expires sooner than this.",
    ),
    row(
        "agents.auth.refresh.tick",
        Duration,
        "\"60s\"",
        Repo,
        false,
        "How often credential expiry is checked.",
    ),
    row(
        "agents.programStatus.enabled",
        Bool,
        "true",
        Repo,
        false,
        "Collect program status from agents.",
    ),
    // runtime
    row(
        "runtime.kind",
        ValueKind::Enum(&["docker", "apple", "sbx", "builtin"]),
        "\"docker\"",
        Global,
        false,
        "Container runtime agents run in; sbx and builtin are experimental.",
    ),
    row(
        "runtime.baseImage",
        ValueKind::String,
        "null",
        Repo,
        false,
        "Base image tag for setup and teardown containers.",
    ),
    row(
        "runtime.dockerfile",
        ValueKind::String,
        "\"Dockerfile.dev\"",
        Repo,
        false,
        "Dockerfile, relative to the repo root, that agent images are built from.",
    ),
    row(
        "runtime.builtin.stateDir",
        ValueKind::String,
        "null",
        Repo,
        false,
        "Absolute private state directory of the builtin runtime (default <AWMAN_HOME>/builtin).",
    ),
    row(
        "runtime.builtin.vcpus",
        int(Some(1), Some(255)),
        "4",
        Repo,
        true,
        "Whole vCPUs per builtin agent VM.",
    ),
    row(
        "runtime.builtin.memory",
        Size,
        "\"4GiB\"",
        Repo,
        false,
        "Guest memory per builtin agent VM (at least 128MiB).",
    ),
    row(
        "runtime.builtin.rootDisk",
        Size,
        "\"30GiB\"",
        Repo,
        false,
        "Writable root disk per builtin agent VM (at least 256MiB).",
    ),
    row(
        "runtime.builtin.mountQuota",
        Size,
        "\"30GiB\"",
        Repo,
        false,
        "How much a builtin guest may add to each host mount (at least 1MiB).",
    ),
    row(
        "runtime.builtin.builder.enabled",
        Bool,
        "false",
        Repo,
        false,
        "Let awman ready build missing builtin images locally.",
    ),
    row(
        "runtime.builtin.imageSource",
        Object,
        "null",
        Repo,
        false,
        "Where awman ready acquires builtin images that are not cached.",
    ),
    row(
        "runtime.builtin.images.*",
        Object,
        "{}",
        Repo,
        false,
        "Per-image source overrides keyed by awman image tag.",
    ),
    row(
        "runtime.builtin.registries.*",
        Object,
        "{}",
        Repo,
        false,
        "Registry host settings keyed by host[:port].",
    ),
    row(
        "runtime.builtin.network.mode",
        ValueKind::Enum(&["none", "allowlist", "public"]),
        "null",
        Repo,
        false,
        "Builtin guest network policy; a repo may only narrow the global one.",
    ),
    row(
        "runtime.builtin.network.allow",
        StringList,
        "null",
        Repo,
        false,
        "Names a builtin guest may reach in allowlist mode.",
    ),
    row(
        "runtime.builtin.network.hostPorts",
        ValueKind::IntList,
        "null",
        Repo,
        false,
        "Host ports a builtin guest may reach.",
    ),
    row(
        "runtime.builtin.network.nameservers",
        StringList,
        "null",
        Repo,
        false,
        "DNS servers a builtin guest uses.",
    ),
    row(
        "runtime.builtin.network.trustHostCas",
        Bool,
        "null",
        Repo,
        false,
        "Install the host's trusted CA certificates in builtin guests.",
    ),
    // overlays
    ConfigKey {
        merge: Merge::Concat,
        ..row(
            "overlays",
            StringList,
            "[]",
            Task,
            false,
            "Overlays applied to every agent container; layers add to each other.",
        )
    },
    // workItems
    row(
        "workItems.dir",
        ValueKind::String,
        "\"aspec/work-items\"",
        Repo,
        true,
        "Work items directory, relative to the repo root.",
    ),
    row(
        "workItems.template",
        ValueKind::String,
        "null",
        Repo,
        false,
        "Work item template (default <dir>/0000-template.md).",
    ),
    // workflows
    row(
        "workflows.maxConcurrentSteps",
        POSITIVE,
        "null",
        Task,
        false,
        "Cap on concurrently running workflow steps (null = unlimited).",
    ),
    row(
        "workflows.prerequisites.preflight",
        ValueKind::Enum(&["warn", "off"]),
        "\"warn\"",
        Task,
        false,
        "Whether workflow prerequisites are checked before a run.",
    ),
    row(
        "workflows.prerequisites.unresolved",
        ValueKind::Enum(&["warn", "fail"]),
        "null",
        Task,
        false,
        "What an unresolved prerequisite does (null = frontend default).",
    ),
    row(
        "workflows.prerequisites.gitIdentity.use",
        CREDENTIAL_USE,
        "\"ask\"",
        Task,
        false,
        "Whether workflows may use a git identity.",
    ),
    row(
        "workflows.prerequisites.gitIdentity.name",
        ValueKind::String,
        "null",
        Task,
        false,
        "Fixed git author name for workflows.",
    ),
    row(
        "workflows.prerequisites.gitIdentity.email",
        ValueKind::String,
        "null",
        Task,
        false,
        "Fixed git author email for workflows.",
    ),
    row(
        "workflows.prerequisites.ssh.use",
        CREDENTIAL_USE,
        "\"ask\"",
        Task,
        false,
        "Whether workflows may use an SSH key.",
    ),
    row(
        "workflows.prerequisites.ssh.key",
        ValueKind::String,
        "null",
        Task,
        false,
        "SSH key under ~/.ssh offered to workflows.",
    ),
    row(
        "workflows.prerequisites.githubToken.use",
        CREDENTIAL_USE,
        "\"ask\"",
        Task,
        false,
        "Whether workflows may use a GitHub token.",
    ),
    row(
        "workflows.prerequisites.githubToken.envVars",
        StringList,
        "[]",
        Task,
        true,
        "Host variables a GitHub token is read from (empty = GH_TOKEN, GITHUB_TOKEN).",
    ),
    row(
        "workflows.prerequisites.githubToken.verify",
        Bool,
        "true",
        Task,
        false,
        "Verify a GitHub token before a run.",
    ),
    row(
        "workflows.prerequisites.githubToken.acquire.argv",
        StringList,
        "null",
        Global,
        false,
        "Command that prints a GitHub token.",
    ),
    row(
        "workflows.prerequisites.githubToken.acquire.timeout",
        Duration,
        "\"30s\"",
        Global,
        false,
        "How long the token command may run (at most 300s).",
    ),
    row(
        "workflows.prerequisites.githubToken.acquire.refresh",
        ValueKind::Enum(&["start", "before_teardown"]),
        "\"start\"",
        Global,
        false,
        "When the token command runs.",
    ),
    row(
        "workflows.prerequisites.allowApiClientGrants",
        Bool,
        "false",
        Global,
        false,
        "Let API clients rely on stored prerequisite grants.",
    ),
    row(
        "workflows.dynamic.defaultLeader",
        ValueKind::String,
        "null",
        Task,
        true,
        "Default dynamic-workflow leader as agent::model.",
    ),
    row(
        "workflows.dynamic.agentsToModels.*",
        StringList,
        "[]",
        Task,
        true,
        "Models a dynamic-workflow leader may schedule for the named agent.",
    ),
    ConfigKey {
        merge: Merge::Concat,
        ..row(
            "workflows.dynamic.guidance",
            StringList,
            "[]",
            Task,
            true,
            "Instructions the dynamic-workflow leader follows; layers add (at most 50).",
        )
    },
    // worktree
    row(
        "worktree.onDirtyMergeTarget",
        ValueKind::Enum(&["ask", "commit", "stash", "fail"]),
        "\"ask\"",
        Repo,
        false,
        "What to do when the merge target has uncommitted changes.",
    ),
    row(
        "worktree.mergeFix.onFailure",
        ValueKind::Enum(&["ask", "agent", "keep"]),
        "\"ask\"",
        Repo,
        false,
        "What to do when merging a worktree branch fails.",
    ),
    row(
        "worktree.mergeFix.promptTemplate",
        ValueKind::String,
        "null",
        Repo,
        false,
        "Prompt template, relative to the repo root, for the merge-fix agent.",
    ),
    // tui
    row(
        "tui.scrollbackLines",
        int(Some(0), None),
        "10000",
        Repo,
        false,
        "Scrollback lines kept per container terminal.",
    ),
    row(
        "tui.backend",
        ValueKind::Enum(&["auto", "daemon", "builtin"]),
        "\"auto\"",
        Repo,
        false,
        "Where the TUI runs commands: automatic selection, the daemon, or the builtin backend.",
    ),
    row(
        "tui.programStatus.emit",
        Bool,
        "true",
        Repo,
        false,
        "Emit program status from the TUI.",
    ),
    row(
        "tui.worker.enabled",
        Bool,
        "false",
        Repo,
        false,
        "Enable the TUI work queue panel (Ctrl+Q) and an instance-owned manual worker.",
    ),
    row(
        "tui.integrations.*",
        Object,
        "{}",
        Repo,
        false,
        "Integration-specific TUI settings keyed by integration name.",
    ),
    row(
        "tui.integrations.*.enabled",
        ValueKind::Enum(&["auto", "on", "off"]),
        "\"auto\"",
        Repo,
        false,
        "Whether this TUI integration is selected automatically, enabled, or disabled.",
    ),
    // daemon
    row(
        "daemon.maxConcurrentJobs",
        POSITIVE,
        "4",
        Global,
        true,
        "Jobs the daemon runs at once.",
    ),
    row(
        "daemon.verboseSetup",
        Bool,
        "false",
        Global,
        true,
        "Log session setup in detail.",
    ),
    row(
        "daemon.terminal.cols",
        POSITIVE,
        "120",
        Global,
        true,
        "Default daemon terminal width.",
    ),
    row(
        "daemon.terminal.rows",
        POSITIVE,
        "40",
        Global,
        true,
        "Default daemon terminal height.",
    ),
    row(
        "daemon.terminal.remoteInput",
        Bool,
        "false",
        Global,
        true,
        "Accept terminal input from network clients.",
    ),
    row(
        "daemon.network.enabled",
        Bool,
        "false",
        Global,
        true,
        "Serve the API on the network listener.",
    ),
    row(
        "daemon.network.listen",
        ValueKind::String,
        "\"127.0.0.1:9876\"",
        Global,
        false,
        "host:port the API listener binds; changing it needs a daemon restart.",
    ),
    row(
        "daemon.network.tlsSans",
        StringList,
        "[]",
        Global,
        true,
        "Extra names in the listener's TLS certificate.",
    ),
    row(
        "daemon.network.features",
        StringList,
        "[\"worker\"]",
        Global,
        true,
        "Features served on the network listener.",
    ),
    row(
        "daemon.features.squad.enabled",
        Bool,
        "false",
        Global,
        true,
        "Run the squad scheduler.",
    ),
    row(
        "daemon.features.squad.interaction",
        INTERACTION,
        "\"interactive\"",
        Global,
        true,
        "How squad evaluations interact.",
    ),
    row(
        "daemon.features.squad.allowOverride",
        Bool,
        "true",
        Global,
        true,
        "Let a squad task choose its own interaction.",
    ),
    row(
        "daemon.features.squad.maxConcurrentJobs",
        POSITIVE,
        "1",
        Global,
        true,
        "Squad runs at once.",
    ),
    row(
        "daemon.features.squad.maxConcurrentEvaluations",
        POSITIVE,
        "2",
        Global,
        true,
        "Squad evaluations at once.",
    ),
    row(
        "daemon.features.squad.envPersistence",
        ValueKind::Enum(&["keychain", "none"]),
        "\"keychain\"",
        Global,
        true,
        "Where the squad daemon persists pushed task environment.",
    ),
    row(
        "daemon.features.worker.enabled",
        Bool,
        "false",
        Global,
        true,
        "Run the worker feature.",
    ),
    row(
        "daemon.features.worker.maxConcurrentJobs",
        POSITIVE,
        "2",
        Global,
        true,
        "Worker jobs at once.",
    ),
    row(
        "daemon.features.worker.push.enabled",
        Bool,
        "true",
        Global,
        true,
        "Accept jobs pushed by API clients.",
    ),
    row(
        "daemon.features.worker.push.interaction",
        INTERACTION,
        "\"headless\"",
        Global,
        true,
        "Default interaction of pushed jobs.",
    ),
    row(
        "daemon.features.worker.push.allowOverride",
        Bool,
        "true",
        Global,
        true,
        "Let a pushed job choose its own interaction.",
    ),
    row(
        "daemon.features.worker.push.workDirs.allow",
        StringList,
        "[]",
        Global,
        true,
        "Directories pushed jobs may run in.",
    ),
    row(
        "daemon.features.worker.push.workDirs.roots",
        StringList,
        "[]",
        Global,
        true,
        "Directories whose children pushed jobs may run in.",
    ),
    row(
        "daemon.features.worker.pull.enabled",
        Bool,
        "true",
        Global,
        true,
        "Register enabled work servers and claim eligible work items.",
    ),
    row(
        "daemon.features.worker.pull.interaction",
        INTERACTION,
        "\"interactive\"",
        Global,
        true,
        "Default interaction for pulled jobs.",
    ),
    row(
        "daemon.features.worker.pull.allowOverride",
        Bool,
        "true",
        Global,
        true,
        "Allow work items and server entries to override pull interaction.",
    ),
    row(
        "daemon.features.worker.pull.workRoot",
        ValueKind::String,
        "null",
        Global,
        true,
        "Root for pulled-job checkouts; null uses <data>/worker.",
    ),
    row(
        "daemon.features.engine.enabled",
        Bool,
        "false",
        Global,
        true,
        "Serve engine instances from the daemon.",
    ),
    row(
        "daemon.features.engine.allowEphemeral",
        Bool,
        "true",
        Global,
        true,
        "Allow the daemon to start the engine feature on demand.",
    ),
    row(
        "daemon.features.engine.idleTimeout",
        Duration,
        "\"15m\"",
        Global,
        true,
        "Idle time after which an unused on-demand engine stops.",
    ),
    row(
        "daemon.features.engine.maxConcurrentJobs",
        POSITIVE,
        "4",
        Global,
        true,
        "Engine jobs the daemon runs at once.",
    ),
    // workServers
    row(
        "workServers.*.url",
        ValueKind::String,
        "null",
        Global,
        true,
        "Base URL of a linked work server; required for every entry.",
    ),
    row(
        "workServers.*.enabled",
        Bool,
        "true",
        Global,
        true,
        "Whether this work server is enabled.",
    ),
    row(
        "workServers.*.claim",
        ValueKind::Enum(&["auto", "manual"]),
        "\"auto\"",
        Global,
        true,
        "Claim automatically or wait for a person to pick work items.",
    ),
    row(
        "workServers.*.maxConcurrentJobs",
        POSITIVE,
        "1",
        Global,
        true,
        "Maximum concurrent jobs claimed from this server.",
    ),
    row(
        "workServers.*.interaction",
        INTERACTION,
        "null",
        Global,
        true,
        "Optional interaction override for this server's jobs.",
    ),
    row(
        "workServers.*.labels",
        StringMap,
        "{}",
        Global,
        true,
        "Owner-configured worker labels for this server.",
    ),
    row(
        "workServers.*.labels.*",
        ValueKind::String,
        "null",
        Global,
        true,
        "One owner-configured worker label; awman/ is reserved.",
    ),
    // remote
    row(
        "remote.default",
        ValueKind::String,
        "null",
        Global,
        false,
        "Target used when --remote names none.",
    ),
    row(
        "remote.targets.*.url",
        ValueKind::String,
        "null",
        Global,
        false,
        "Target API URL (https://host:port); required for every entry.",
    ),
    ConfigKey {
        sensitive: true,
        ..row(
            "remote.targets.*.apiKey",
            ValueKind::String,
            "null",
            Global,
            false,
            "Bearer API key for the target.",
        )
    },
];

/// Old leaf path → new leaf path (+ an example value when the type changed).
///
/// Read only by [`KeyError::Unknown`] construction. Never consulted for
/// input: an old key fails validation like any unknown key, and the hint
/// only tells the user where it went. Delete this table one release after
/// the item that introduced it ships.
pub const MOVED_KEYS: &[(&str, &str, Option<&str>)] = &[
    ("default_agent", "agents.default", None),
    ("agent", "agents.default", None),
    ("agentStuckTimeout", "agents.stuckTimeout", Some("\"30s\"")),
    ("launchMode", "agents.launchMode", None),
    ("launchModeFallback", "agents.acpFallback", None),
    ("agentFlags", "agents.flags", None),
    ("yoloDisallowedTools", "agents.yolo.disallowedTools", None),
    ("auth", "agents.auth.mode", None),
    ("authRefresh", "agents.auth.refresh", None),
    (
        "authRefresh.thresholdMinutes",
        "agents.auth.refresh.threshold",
        Some("\"20m\""),
    ),
    (
        "authRefresh.tickSeconds",
        "agents.auth.refresh.tick",
        Some("\"60s\""),
    ),
    (
        "runtime",
        "runtime.kind",
        Some("\"docker\" | \"apple\" | \"sbx\" | \"builtin\""),
    ),
    ("baseImage", "runtime.baseImage", None),
    ("dockerfile", "runtime.dockerfile", None),
    ("builtin", "runtime.builtin", None),
    (
        "builtin.memoryMib",
        "runtime.builtin.memory",
        Some("\"4GiB\""),
    ),
    (
        "builtin.rootDiskMib",
        "runtime.builtin.rootDisk",
        Some("\"30GiB\""),
    ),
    (
        "builtin.mountQuotaMib",
        "runtime.builtin.mountQuota",
        Some("\"30GiB\""),
    ),
    ("work_items", "workItems", None),
    ("maxConcurrentAgents", "workflows.maxConcurrentSteps", None),
    (
        "workflows.dynamic.maxConcurrentSteps",
        "workflows.maxConcurrentSteps",
        None,
    ),
    ("dynamicWorkflows", "workflows.dynamic", None),
    (
        "workflows.prerequisites.githubToken.acquire.timeoutSecs",
        "workflows.prerequisites.githubToken.acquire.timeout",
        Some("\"30s\""),
    ),
    ("terminal_scrollback_lines", "tui.scrollbackLines", None),
    ("workers", "daemon.maxConcurrentJobs", None),
    ("api", "daemon.features.worker.push", None),
    (
        "api.bind",
        "daemon.network.listen",
        Some("\"0.0.0.0:9876\""),
    ),
    ("api.tls", "daemon.network.tlsSans", None),
    ("api.tlsSans", "daemon.network.tlsSans", None),
    (
        "api.key",
        "(removed) use `awman daemon api keys.create`",
        None,
    ),
    (
        "api.apiKey",
        "(removed) use `awman daemon api keys.create`",
        None,
    ),
    (
        "api.workDirs",
        "daemon.features.worker.push.workDirs.allow",
        None,
    ),
    (
        "api.workDirRoots",
        "daemon.features.worker.push.workDirs.roots",
        None,
    ),
    (
        "api.alwaysNonInteractive",
        "daemon.features.worker.push.allowOverride",
        Some("false (with interaction: \"headless\")"),
    ),
    (
        "api.port",
        "daemon.network.listen",
        Some("\"127.0.0.1:9876\""),
    ),
    (
        "squad.maxConcurrentEvaluations",
        "daemon.features.squad.maxConcurrentEvaluations",
        None,
    ),
    ("squad", "daemon.features.squad", None),
    (
        "squad.port",
        "(removed) squad is served by the daemon's local listener",
        None,
    ),
    (
        "squad.bind",
        "(removed) squad is served by the daemon's local listener",
        None,
    ),
    (
        "squad.envPersistence",
        "daemon.features.squad.envPersistence",
        None,
    ),
    (
        "squad.agentsToModels",
        "workflows.dynamic.agentsToModels",
        None,
    ),
    (
        "squad.defaultLeader",
        "workflows.dynamic.defaultLeader",
        None,
    ),
    ("squad.guidance", "workflows.dynamic.guidance", None),
    ("squad.workflows", "workflows", None),
    (
        "remote.defaultAddr",
        "remote.targets.<name>.url (and remote.default = <name>)",
        None,
    ),
    ("remote.defaultAPIKey", "remote.targets.<name>.apiKey", None),
    (
        "worker.workRoot",
        "daemon.features.worker.pull.workRoot",
        None,
    ),
    ("worker.servers", "workServers", None),
    ("remote.savedDirs", "(removed)", None),
    (
        "auto_agent_auth_accepted",
        "(state, not config: <AWMAN_HOME>/data/repos/<hash>/state.json)",
        None,
    ),
    ("envPassthrough", "overlays (env(VAR) entries)", None),
];

/// The hint for an unknown path: the longest [`MOVED_KEYS`] entry that is the
/// path itself or one of its prefixes. `""` when none matches.
pub fn moved_hint(path: &str) -> String {
    moved_entry(path).map(render_moved).unwrap_or_default()
}

type MovedEntry = (&'static str, &'static str, Option<&'static str>);

fn moved_entry(path: &str) -> Option<&'static MovedEntry> {
    MOVED_KEYS
        .iter()
        .filter(|(old, _, _)| path == *old || path.starts_with(&format!("{old}.")))
        .max_by_key(|(old, _, _)| old.len())
}

fn render_moved(entry: &MovedEntry) -> String {
    let (old, new, example) = entry;
    match example {
        Some(example) => format!("; `{old}` moved to `{new}` ({example})"),
        None if new.starts_with('(') => format!("; `{old}` was removed {new}"),
        None => format!("; `{old}` moved to `{new}`"),
    }
}

/// The hint for an unknown key holding `value`: the most specific
/// [`MOVED_KEYS`] entries for the key and every path inside it, so
/// `{"squad": {"guidance": […]}}` names `workflows.dynamic.guidance`, not
/// just the block.
pub fn moved_hint_for_value(path: &str, value: &Value) -> String {
    let mut paths = vec![path.to_string()];
    let mut stack = vec![(path.to_string(), value)];
    while let Some((prefix, node)) = stack.pop() {
        if let Some(object) = node.as_object() {
            for (name, child) in object {
                let child_path = format!("{prefix}.{name}");
                paths.push(child_path.clone());
                stack.push((child_path, child));
            }
        }
    }
    let own = moved_entry(path);
    let mut entries: Vec<&MovedEntry> = Vec::new();
    for p in &paths {
        if let Some(entry) = moved_entry(p) {
            if !entries.iter().any(|e| e.0 == entry.0) {
                entries.push(entry);
            }
        }
    }
    // Prefer the specific entries inside the block over the block's own.
    if entries.len() > 1 {
        entries.retain(|e| Some(*e) != own);
    }
    entries.sort_by_key(|e| e.0);
    entries.into_iter().map(render_moved).collect()
}

/// A key-table error. Every config validation message comes from here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("unknown config key `{path}`{location}; valid keys here: {}{moved}", siblings.join(", "))]
    Unknown {
        path: String,
        /// `""` or ` in <file>`.
        location: String,
        siblings: Vec<&'static str>,
        /// `""` or `; \`old\` moved to \`new\` ("30s")`.
        moved: String,
    },
    #[error("`{path}` may only be set in {allowed} (found in {found})")]
    Scope {
        path: String,
        allowed: &'static str,
        found: &'static str,
    },
    #[error("invalid value for `{path}`: {reason}")]
    Value { path: String, reason: String },
    #[error("invalid name `{name}` in `{path}`: must match {MAP_NAME}")]
    Name { path: String, name: String },
    #[error("`{a}` and `{b}` differ only by `-` versus `_`")]
    DuplicateName { a: String, b: String },
    #[error("`{path}` is an object; edit it in the config file")]
    ObjectNotSettable { path: String },
}

impl KeyError {
    /// An unknown-key error for `path` (concrete), listing what is valid next
    /// to it and the moved-to hint.
    pub fn unknown(path: &str) -> Self {
        let parent = path.rsplit_once('.').map(|(p, _)| p).unwrap_or("");
        Self::Unknown {
            path: path.to_string(),
            location: String::new(),
            siblings: siblings(parent),
            moved: moved_hint(path),
        }
    }

    /// Attach the file the error was found in.
    pub fn in_file(mut self, file: &std::path::Path) -> Self {
        if let Self::Unknown { location, .. } = &mut self {
            *location = format!(" in {}", file.display());
        }
        self
    }
}

impl ConfigKey {
    pub fn default_value(&self) -> Value {
        serde_json::from_str(self.default).expect("key table defaults are valid JSON")
    }

    /// Whether the key has an environment-variable form (objects do not).
    pub fn has_env(&self) -> bool {
        self.kind != ValueKind::Object && self.path != "workServers.*.labels.*"
    }

    /// `AWMAN_` + SCREAMING_SNAKE(path), `*` kept as a placeholder:
    /// `agents.stuckTimeout` → `AWMAN_AGENTS_STUCK_TIMEOUT`,
    /// `workServers.*.url` → `AWMAN_WORK_SERVERS_*_URL`.
    pub fn env_pattern(&self) -> String {
        let mut out = String::from("AWMAN");
        for segment in self.path.split('.') {
            out.push('_');
            out.push_str(&screaming_snake(segment));
        }
        out
    }

    /// The concrete env name for a map entry: each `*` → upper(name) with
    /// `-` → `_`, in order.
    pub fn env_name(&self, names: &[&str]) -> String {
        let mut names = names.iter();
        let mut out = String::from("AWMAN");
        for segment in self.path.split('.') {
            out.push('_');
            if segment == "*" {
                let name = names.next().copied().unwrap_or("*");
                out.push_str(&name.to_ascii_uppercase().replace('-', "_"));
            } else {
                out.push_str(&screaming_snake(segment));
            }
        }
        out
    }

    /// Whether the path contains a user-chosen `*` segment.
    pub fn is_map_member(&self) -> bool {
        self.path.split('.').any(|s| s == "*")
    }

    /// First path segment.
    pub fn section(&self) -> &'static str {
        self.path.split('.').next().unwrap_or(self.path)
    }

    /// Whether `*` segments of this row are free-form (any non-empty JSON
    /// key) rather than [`MAP_NAME`]. This is used for image tags
    /// (`awman-x:latest`), registry hosts (`ghcr.io`) and the namespaced key
    /// under `workServers.*.labels` (which has no per-key env form).
    fn free_form_names(&self) -> bool {
        matches!(
            self.path,
            "runtime.builtin.images.*" | "runtime.builtin.registries.*" | "workServers.*.labels.*"
        )
    }
}

fn screaming_snake(segment: &str) -> String {
    let mut out = String::new();
    for (i, c) in segment.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

/// A concrete path bound to its row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    pub key: &'static ConfigKey,
    /// The concrete path as given.
    pub path: String,
    /// Bound `*` segments, in order.
    pub names: Vec<String>,
}

/// Whether a concrete path matches a row pattern, collecting `*` bindings.
fn match_pattern(pattern: &str, segments: &[&str]) -> Option<Vec<String>> {
    let parts: Vec<&str> = pattern.split('.').collect();
    if parts.len() != segments.len() {
        return None;
    }
    let mut names = Vec::new();
    for (p, s) in parts.iter().zip(segments) {
        if *p == "*" {
            names.push((*s).to_string());
        } else if p != s {
            return None;
        }
    }
    Some(names)
}

/// Split a concrete dotted path into its segments. A free-form object-map
/// member (an image tag or a registry host such as `ghcr.io`) may itself
/// contain dots, so everything after such a map's prefix is one segment.
pub fn path_segments(path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pattern = String::new();
    let mut rest = path;
    while !rest.is_empty() {
        let star = join(&pattern, "*");
        if CONFIG_KEYS
            .iter()
            .any(|k| k.path == star && k.free_form_names())
        {
            out.push(rest.to_string());
            return out;
        }
        let (segment, tail) = rest.split_once('.').unwrap_or((rest, ""));
        let literal = join(&pattern, segment);
        pattern = if is_known_pattern(&literal) {
            literal
        } else {
            star
        };
        out.push(segment.to_string());
        rest = tail;
    }
    out
}

/// Resolve a concrete dotted path (`workServers.infact.url`) to its row and
/// the bound `*` names. Rejects names that fail [`MAP_NAME`].
pub fn resolve(path: &str) -> Result<ResolvedKey, KeyError> {
    let owned = path_segments(path);
    let segments: Vec<&str> = owned.iter().map(String::as_str).collect();
    for key in CONFIG_KEYS {
        if let Some(names) = match_pattern(key.path, &segments) {
            if key.path == "workServers.*.labels.*" {
                if !is_valid_map_name(&names[0]) || names[1].is_empty() {
                    return Err(KeyError::Name {
                        path: path.to_string(),
                        name: names
                            .iter()
                            .find(|n| !is_valid_map_name(n))
                            .cloned()
                            .unwrap_or_default(),
                    });
                }
            } else if !key.free_form_names() {
                if let Some(bad) = names.iter().find(|n| !is_valid_map_name(n)) {
                    return Err(KeyError::Name {
                        path: path.to_string(),
                        name: bad.clone(),
                    });
                }
            }
            return Ok(ResolvedKey {
                key,
                path: path.to_string(),
                names,
            });
        }
    }
    Err(KeyError::unknown(path))
}

/// Whether a concrete config key is applied by a running daemon on reload.
/// Unknown keys return `false` (callers can use [`resolve`] for diagnostics).
pub fn is_live(path: &str) -> bool {
    resolve(path).is_ok_and(|resolved| resolved.key.live)
}

/// Whether `pattern` (a row path, or a prefix of one) has children, i.e. is a
/// section or map rather than a leaf.
fn pattern_is_branch(pattern: &str) -> bool {
    let prefix = format!("{pattern}.");
    CONFIG_KEYS.iter().any(|k| k.path.starts_with(&prefix))
}

/// The row-pattern form of a concrete path prefix: map-name positions become
/// `*`. `None` when the prefix names nothing in the table.
pub fn pattern_of(concrete: &str) -> Option<String> {
    if concrete.is_empty() {
        return Some(String::new());
    }
    let mut pattern = String::new();
    for segment in concrete.split('.') {
        let literal = join(&pattern, segment);
        let star = join(&pattern, "*");
        pattern = if is_known_pattern(&literal) {
            literal
        } else if is_known_pattern(&star) {
            star
        } else {
            return None;
        };
    }
    Some(pattern)
}

fn join(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}.{segment}")
    }
}

fn is_known_pattern(pattern: &str) -> bool {
    CONFIG_KEYS.iter().any(|k| k.path == pattern) || pattern_is_branch(pattern)
}

/// Valid children of a concrete object path, for unknown-key errors. A map
/// level answers `<name>`.
pub fn siblings(path_prefix: &str) -> Vec<&'static str> {
    let Some(pattern) = pattern_of(path_prefix) else {
        return Vec::new();
    };
    let prefix = if pattern.is_empty() {
        String::new()
    } else {
        format!("{pattern}.")
    };
    let mut out: Vec<&'static str> = CONFIG_KEYS
        .iter()
        .filter_map(|k| k.path.strip_prefix(prefix.as_str()))
        .map(|rest| rest.split('.').next().unwrap_or(rest))
        .map(|child| if child == "*" { "<name>" } else { child })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Resolve an environment variable name to a key (and bound names).
///
/// `known_names` answers the map keys present in the merged file config for
/// a map's section path (`workServers`, `remote.targets`), so a `*` segment
/// splits unambiguously: the middle is compared case-insensitively with `-`
/// and `_` folded. No match → a new entry named by the lower-cased middle.
pub fn resolve_env(var: &str, known_names: &dyn Fn(&str) -> Vec<String>) -> Option<ResolvedKey> {
    let mut best: Option<(bool, usize, ResolvedKey)> = None;
    for key in CONFIG_KEYS.iter().filter(|k| k.has_env()) {
        let pattern = key.env_pattern();
        let Some((prefix, suffix)) = pattern.split_once('*') else {
            if pattern == var {
                return Some(ResolvedKey {
                    key,
                    path: key.path.to_string(),
                    names: Vec::new(),
                });
            }
            continue;
        };
        if suffix.contains('*') || !var.starts_with(prefix) || !var.ends_with(suffix) {
            continue;
        }
        if var.len() <= prefix.len() + suffix.len() {
            continue;
        }
        let middle = &var[prefix.len()..var.len() - suffix.len()];
        let section = key.path.split(".*").next().unwrap_or("");
        let known = known_names(section)
            .into_iter()
            .find(|n| fold_map_name(n).eq_ignore_ascii_case(middle));
        let matched_known = known.is_some();
        let name = known.unwrap_or_else(|| middle.to_ascii_lowercase());
        let path = key.path.replacen('*', &name, 1);
        let candidate = ResolvedKey {
            key,
            path,
            names: vec![name],
        };
        // Prefer a known name, then the longest literal suffix.
        let rank = (matched_known, suffix.len());
        if best.as_ref().is_none_or(|(k, s, _)| rank > (*k, *s)) {
            best = Some((rank.0, rank.1, candidate));
        }
    }
    best.map(|(_, _, r)| r)
}

/// Parse scalar/list input (`--set`, env, `config set`) by `kind`. The
/// literal `null` resets the key to its default.
pub fn parse_input(key: &ConfigKey, raw: &str) -> Result<Value, KeyError> {
    let invalid = |reason: String| KeyError::Value {
        path: key.path.to_string(),
        reason,
    };
    let trimmed = raw.trim();
    if trimmed == "null" {
        return Ok(Value::Null);
    }
    let value = match key.kind {
        ValueKind::Bool => match trimmed {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            other => return Err(invalid(format!("expected true or false, got {other:?}"))),
        },
        ValueKind::Int { .. } => {
            let n: i64 = trimmed
                .parse()
                .map_err(|_| invalid(format!("expected a whole number, got {trimmed:?}")))?;
            Value::from(n)
        }
        ValueKind::String | ValueKind::Enum(_) | ValueKind::Duration | ValueKind::Size => {
            Value::String(trimmed.to_string())
        }
        ValueKind::StringList => Value::Array(
            split_list(trimmed)
                .map(|s| Value::String(s.to_string()))
                .collect(),
        ),
        ValueKind::IntList => Value::Array(
            split_list(trimmed)
                .map(|s| {
                    s.parse::<i64>()
                        .map(Value::from)
                        .map_err(|_| invalid(format!("{s:?} is not a whole number")))
                })
                .collect::<Result<_, _>>()?,
        ),
        ValueKind::StringMap => {
            let mut map = serde_json::Map::new();
            for pair in split_list(trimmed) {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| invalid(format!("expected key=value, got {pair:?}")))?;
                map.insert(k.trim().to_string(), Value::String(v.trim().to_string()));
            }
            Value::Object(map)
        }
        ValueKind::Object => {
            return Err(KeyError::ObjectNotSettable {
                path: key.path.to_string(),
            });
        }
    };
    validate_value(key, &value)?;
    Ok(value)
}

fn split_list(raw: &str) -> impl Iterator<Item = &str> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// Validate a file-shaped JSON value for a row (type + bounds + enum set).
/// `null` is always accepted: it means "not set here".
pub fn validate_value(key: &ConfigKey, value: &Value) -> Result<(), KeyError> {
    let invalid = |reason: String| KeyError::Value {
        path: key.path.to_string(),
        reason,
    };
    let shown = if key.sensitive {
        "<hidden: sensitive value>".to_string()
    } else {
        value.to_string()
    };
    if value.is_null() {
        return Ok(());
    }
    match key.kind {
        ValueKind::Bool => {
            if !value.is_boolean() {
                return Err(invalid(format!("expected true or false, got {shown}")));
            }
        }
        ValueKind::Int { min, max } => {
            let n = value
                .as_i64()
                .ok_or_else(|| invalid(format!("expected a whole number, got {shown}")))?;
            if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                return Err(invalid(format!(
                    "{n} is out of range ({})",
                    key.kind.describe()
                )));
            }
        }
        ValueKind::String => {
            if !value.is_string() {
                return Err(invalid(format!("expected a string, got {shown}")));
            }
            if key.path == "daemon.network.listen" {
                super::network::ListenAddr::parse(value.as_str().unwrap())
                    .map_err(|e| invalid(e.to_string()))?;
            }
        }
        ValueKind::Enum(values) => {
            let s = value.as_str().unwrap_or_default();
            if !values.contains(&s) {
                return Err(invalid(format!(
                    "expected one of {}, got {shown}",
                    values.join(", ")
                )));
            }
        }
        ValueKind::Duration => {
            let s = value.as_str().ok_or_else(|| {
                invalid(format!(
                    "expected a duration string such as \"30s\", got {shown}"
                ))
            })?;
            s.parse::<super::units::DurationStr>().map_err(invalid)?;
        }
        ValueKind::Size => {
            let s = value.as_str().ok_or_else(|| {
                invalid(format!(
                    "expected a size string such as \"4GiB\", got {shown}"
                ))
            })?;
            s.parse::<super::units::Size>().map_err(invalid)?;
        }
        ValueKind::StringList => {
            let ok = value.as_array().is_some_and(|items| {
                items.iter().all(|item| {
                    let Some(s) = item.as_str() else { return false };
                    match key.path {
                        "daemon.network.tlsSans" => super::network::TlsSan::parse(s).is_ok(),
                        "daemon.network.features" => ["squad", "worker", "engine"].contains(&s),
                        _ => true,
                    }
                })
            });
            if !ok {
                let expected = match key.path {
                    "daemon.network.tlsSans" => "a list of DNS names or IP literals",
                    "daemon.network.features" => "a list containing only squad, worker, or engine",
                    _ => "a list of strings",
                };
                return Err(invalid(format!("expected {expected}, got {shown}")));
            }
        }
        ValueKind::IntList => {
            let ok = value
                .as_array()
                .is_some_and(|a| a.iter().all(|v| v.as_i64().is_some()));
            if !ok {
                return Err(invalid(format!(
                    "expected a list of whole numbers, got {shown}"
                )));
            }
        }
        ValueKind::StringMap => {
            let ok = value
                .as_object()
                .is_some_and(|m| m.values().all(Value::is_string));
            if !ok {
                return Err(invalid(format!(
                    "expected an object of strings, got {shown}"
                )));
            }
        }
        ValueKind::Object => {
            if !value.is_object() {
                return Err(invalid(format!("expected an object, got {shown}")));
            }
        }
    }
    Ok(())
}

/// Walk a whole config file's JSON and check every key against the table:
/// unknown keys (with siblings and moved-to hints), map names, `-`/`_`
/// duplicates, value shapes and scope for `kind`.
pub fn validate_document(document: &Value, kind: FileKind) -> Result<(), KeyError> {
    let Some(object) = document.as_object() else {
        return Err(KeyError::Value {
            path: "(root)".into(),
            reason: format!("a config file must be a JSON object, got {document}"),
        });
    };
    walk(object, "", "", kind)
}

fn walk(
    object: &serde_json::Map<String, Value>,
    concrete: &str,
    pattern: &str,
    kind: FileKind,
) -> Result<(), KeyError> {
    let star = join(pattern, "*");
    let star_row = CONFIG_KEYS.iter().find(|k| k.path == star);
    let is_map_level = pattern_is_branch(&star) || star_row.is_some();
    if is_map_level {
        let free_form = star_row.is_some_and(ConfigKey::free_form_names);
        let mut folded: BTreeMap<String, &str> = BTreeMap::new();
        for name in object.keys() {
            let path = join(concrete, name);
            if free_form {
                if name.is_empty() {
                    return Err(KeyError::Name {
                        path,
                        name: name.clone(),
                    });
                }
            } else {
                if !is_valid_map_name(name) {
                    return Err(KeyError::Name {
                        path,
                        name: name.clone(),
                    });
                }
                if let Some(previous) = folded.insert(fold_map_name(name), name) {
                    return Err(KeyError::DuplicateName {
                        a: join(concrete, previous),
                        b: path,
                    });
                }
            }
        }
    }
    for (name, value) in object {
        let path = join(concrete, name);
        let child = if is_map_level {
            star.clone()
        } else {
            join(pattern, name)
        };
        if let Some(key) = CONFIG_KEYS.iter().find(|k| k.path == child) {
            if !kind.admits(key.scope) {
                return Err(KeyError::Scope {
                    path,
                    allowed: key.scope.allowed_files(),
                    found: kind.as_str(),
                });
            }
            validate_value(key, value).map_err(|e| match e {
                KeyError::Value { reason, .. } => KeyError::Value {
                    path: path.clone(),
                    reason,
                },
                other => other,
            })?;
            if child == "workServers.*.labels.*" {
                let label = path.rsplit('.').next().unwrap_or_default();
                super::work_servers::validate_configured_label(label).map_err(|reason| {
                    KeyError::Value {
                        path: path.clone(),
                        reason,
                    }
                })?;
            }
            // Integration-owned options remain opaque, but the shared
            // `enabled` leaf must still be checked by its declared enum row.
            if child == "tui.integrations.*" {
                if let Some(enabled) = value.get("enabled") {
                    let enabled_key = CONFIG_KEYS
                        .iter()
                        .find(|candidate| candidate.path == "tui.integrations.*.enabled")
                        .expect("integration enabled key is in the key table");
                    validate_value(enabled_key, enabled).map_err(|e| match e {
                        KeyError::Value { reason, .. } => KeyError::Value {
                            path: join(&path, "enabled"),
                            reason,
                        },
                        other => other,
                    })?;
                }
            }
            continue;
        }
        if !pattern_is_branch(&child) {
            let mut error = KeyError::unknown(&path);
            if let KeyError::Unknown { moved, .. } = &mut error {
                *moved = moved_hint_for_value(&path, value);
            }
            return Err(error);
        }
        match value {
            Value::Null => {}
            Value::Object(inner) => walk(inner, &path, &child, kind)?,
            other => {
                // An old scalar key whose name is now a section (`runtime`).
                let hint = moved_hint(&path);
                return Err(KeyError::Value {
                    reason: format!("expected an object, got {other}{hint}"),
                    path,
                });
            }
        }
    }
    Ok(())
}

/// Check the map-name invariants (valid names, no two names that differ only
/// by `-` versus `_`) over a merged document. Layers are validated one file
/// at a time, so a collision between layers, or one introduced by an
/// override, is only visible here.
pub fn validate_map_names(document: &Value) -> Result<(), KeyError> {
    match document.as_object() {
        Some(object) => check_names(object, "", ""),
        None => Ok(()),
    }
}

fn check_names(
    object: &serde_json::Map<String, Value>,
    concrete: &str,
    pattern: &str,
) -> Result<(), KeyError> {
    let star = join(pattern, "*");
    let star_row = CONFIG_KEYS.iter().find(|k| k.path == star);
    let is_map_level = pattern_is_branch(&star) || star_row.is_some();
    if is_map_level && !star_row.is_some_and(ConfigKey::free_form_names) {
        let mut folded: BTreeMap<String, &str> = BTreeMap::new();
        for name in object.keys() {
            let path = join(concrete, name);
            if !is_valid_map_name(name) {
                return Err(KeyError::Name {
                    path,
                    name: name.clone(),
                });
            }
            if let Some(previous) = folded.insert(fold_map_name(name), name) {
                return Err(KeyError::DuplicateName {
                    a: join(concrete, previous),
                    b: path,
                });
            }
        }
    }
    for (name, value) in object {
        let child = if is_map_level {
            star.clone()
        } else {
            join(pattern, name)
        };
        if let Value::Object(inner) = value {
            if pattern_is_branch(&child) {
                check_names(inner, &join(concrete, name), &child)?;
            }
        }
    }
    Ok(())
}

/// The Markdown key table embedded in `docs/07-configuration.md` between
/// `<!-- key-table:start -->` and `<!-- key-table:end -->`.
pub fn render_docs_table() -> String {
    let mut out = String::from(
        "| Key | Type | Default | Scope | Sensitive | Live | Env | Meaning |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    for key in CONFIG_KEYS {
        let merge = if key.merge == Merge::Concat {
            " (additive)"
        } else {
            ""
        };
        let env = if key.has_env() {
            format!("`{}`", key.env_pattern().replace('*', "<NAME>"))
        } else {
            "—".into()
        };
        out.push_str(&format!(
            "| `{}`{merge} | {} | `{}` | {} | {} | {} | {env} | {} |\n",
            key.path,
            key.kind.describe(),
            key.default,
            key.scope.as_str(),
            if key.sensitive { "yes" } else { "no" },
            if key.live { "yes" } else { "no" },
            key.doc,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(key: &ConfigKey) -> impl Iterator<Item = &'static str> {
        key.path.split('.')
    }

    #[test]
    fn every_path_is_camel_case_without_units() {
        const UNIT_SUFFIXES: &[&str] = &[
            "Secs", "Seconds", "Minutes", "Mins", "Hours", "Ms", "Millis", "Mib", "Kib", "Gib",
            "Mb", "Gb", "Bytes",
        ];
        for key in CONFIG_KEYS {
            for segment in segments(key) {
                if segment == "*" {
                    continue;
                }
                let mut chars = segment.chars();
                assert!(
                    chars.next().is_some_and(|c| c.is_ascii_lowercase())
                        && chars.all(|c| c.is_ascii_alphanumeric()),
                    "{} segment {segment:?} is not camelCase",
                    key.path
                );
                for unit in UNIT_SUFFIXES {
                    assert!(
                        !segment.ends_with(unit),
                        "{} carries a unit ({unit}) in its name; put it in the value",
                        key.path
                    );
                }
            }
        }
    }

    #[test]
    fn no_abbreviations_except_id_url_tls_api() {
        // Abbreviations a key must spell out. `id`, `url`, `tls` and `api`
        // are the sanctioned exceptions and are not listed.
        const FORBIDDEN: &[&str] = &[
            "cfg",
            "conf",
            "addr",
            "num",
            "cnt",
            "ctx",
            "tmp",
            "msg",
            "idx",
            "secs",
            "mins",
            "pwd",
            "opts",
            "params",
            "max",
            "min",
            "concurrent",
            "acp",
            "vcpus",
            "cols",
            "rows",
            "env",
            "sans",
        ];
        // Segments the work item's own schema (and design §1) spells with an
        // abbreviation: protocol and domain terms with no clearer spelling.
        // Adding a name here is a contract change, not a test tweak.
        const APPROVED: &[&str] = &[
            "acpFallback",
            "vcpus",
            "cols",
            "rows",
            "envVars",
            "envPersistence",
            "tlsSans",
        ];
        for key in CONFIG_KEYS {
            for segment in segments(key).filter(|s| *s != "*") {
                if APPROVED.contains(&segment) {
                    continue;
                }
                let words = screaming_snake(segment).to_ascii_lowercase();
                for word in words.split('_') {
                    // `max`/`concurrent` only appear inside `maxConcurrent<Noun>`.
                    if (word == "max" || word == "concurrent")
                        && segment.starts_with("maxConcurrent")
                    {
                        continue;
                    }
                    assert!(
                        !FORBIDDEN.contains(&word),
                        "{} uses the abbreviation {word:?}",
                        key.path
                    );
                }
            }
        }
    }

    #[test]
    fn toggles_are_named_enabled() {
        for key in CONFIG_KEYS.iter().filter(|k| k.kind == ValueKind::Bool) {
            let last = key.path.rsplit('.').next().unwrap();
            let lower = last.to_ascii_lowercase();
            // Toggles at a distance are forbidden except where the work item's
            // schema names them (`allowOverride`, `allowEphemeral`) and the
            // credential-policy gate in design §1.8.
            const APPROVED: &[&str] = &["allowOverride", "allowEphemeral", "allowApiClientGrants"];
            let distant = ["use", "allow", "disable", "enable"].iter().any(|p| {
                last.starts_with(p)
                    && last.len() > p.len()
                    && last[p.len()..].starts_with(char::is_uppercase)
            });
            if distant {
                assert!(
                    APPROVED.contains(&last),
                    "{}: toggles are `enabled` inside the thing, not `{last}`",
                    key.path
                );
            }
            if lower.contains("enable") || lower.contains("disable") || lower == "on" {
                assert_eq!(last, "enabled", "{}: toggles are named `enabled`", key.path);
            }
        }
    }

    #[test]
    fn concurrency_keys_are_max_concurrent_noun() {
        for key in CONFIG_KEYS {
            for segment in segments(key) {
                let lower = segment.to_ascii_lowercase();
                if lower.contains("concurren")
                    || lower.contains("workers")
                    || lower.contains("parallel")
                {
                    let noun = segment.strip_prefix("maxConcurrent").unwrap_or_else(|| {
                        panic!("{}: concurrency keys are maxConcurrent<Noun>", key.path)
                    });
                    assert!(
                        noun.starts_with(|c: char| c.is_ascii_uppercase()) && noun.ends_with('s'),
                        "{}: {noun:?} is not a plural noun",
                        key.path
                    );
                }
            }
        }
    }

    #[test]
    fn every_row_is_complete() {
        for key in CONFIG_KEYS {
            let default = key.default_value();
            assert!(!key.doc.trim().is_empty(), "{} has no doc", key.path);
            assert!(key.doc.ends_with('.'), "{} doc is not a sentence", key.path);
            validate_value(key, &default)
                .unwrap_or_else(|e| panic!("{} default does not validate: {e}", key.path));
            if key.merge == Merge::Concat {
                assert_eq!(key.kind, ValueKind::StringList, "{}", key.path);
            }
        }
    }

    #[test]
    fn paths_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for key in CONFIG_KEYS {
            assert!(seen.insert(key.path), "duplicate row {}", key.path);
        }
    }

    #[test]
    fn derived_env_names_are_unique_and_not_reserved() {
        let mut seen = std::collections::HashSet::new();
        for key in CONFIG_KEYS.iter().filter(|k| k.has_env()) {
            let env = key.env_pattern();
            assert!(seen.insert(env.clone()), "{env} derived twice");
            assert!(!RESERVED_ENV.contains(&env.as_str()), "{env} is reserved");
            assert!(
                !env.starts_with("AWMAN_TEST_"),
                "{env} is in the test namespace"
            );
            // A map row could expand to a reserved name: `AWMAN_<prefix>*<suffix>`
            // must not be able to produce one.
            if let Some((prefix, suffix)) = env.split_once('*') {
                for reserved in RESERVED_ENV {
                    assert!(
                        !(reserved.starts_with(prefix)
                            && reserved.ends_with(suffix)
                            && reserved.len() > prefix.len() + suffix.len()),
                        "{env} can expand to reserved {reserved}"
                    );
                }
            }
        }
        for section in CONFIG_KEYS.iter().map(ConfigKey::section) {
            assert!(
                !["home", "test"].contains(&section),
                "top-level `{section}` is reserved"
            );
        }
    }

    #[test]
    fn env_names_derive_from_the_path() {
        let key = resolve("agents.stuckTimeout").unwrap().key;
        assert_eq!(key.env_pattern(), "AWMAN_AGENTS_STUCK_TIMEOUT");
        let key = resolve("workServers.in-fact.url").unwrap().key;
        assert_eq!(key.env_pattern(), "AWMAN_WORK_SERVERS_*_URL");
        assert_eq!(key.env_name(&["in-fact"]), "AWMAN_WORK_SERVERS_IN_FACT_URL");
        assert_eq!(
            resolve("overlays").unwrap().key.env_pattern(),
            "AWMAN_OVERLAYS"
        );
    }

    #[test]
    fn only_these_rows_are_sensitive() {
        let sensitive: Vec<_> = CONFIG_KEYS
            .iter()
            .filter(|k| k.sensitive)
            .map(|k| k.path)
            .collect();
        assert_eq!(sensitive, ["remote.targets.*.apiKey"]);
    }

    #[test]
    fn only_overlays_and_guidance_are_additive() {
        let concat: Vec<_> = CONFIG_KEYS
            .iter()
            .filter(|k| k.merge == Merge::Concat)
            .map(|k| k.path)
            .collect();
        assert_eq!(concat, ["overlays", "workflows.dynamic.guidance"]);
    }

    #[test]
    fn resolve_binds_names_and_rejects_bad_ones() {
        let r = resolve("remote.targets.prod.apiKey").unwrap();
        assert_eq!(r.key.path, "remote.targets.*.apiKey");
        assert_eq!(r.names, ["prod"]);
        assert!(matches!(
            resolve("remote.targets.Prod.apiKey"),
            Err(KeyError::Name { .. })
        ));
        assert!(matches!(
            resolve("workServers._x.url"),
            Err(KeyError::Name { .. })
        ));
        assert!(matches!(
            resolve("agents.nope"),
            Err(KeyError::Unknown { .. })
        ));
    }

    #[test]
    fn map_name_rule() {
        for ok in ["a", "0", "in-fact", "prod_2", &"a".repeat(63)] {
            assert!(is_valid_map_name(ok), "{ok}");
        }
        for bad in ["", "-a", "_a", "A", "a.b", "a b", &"a".repeat(64)] {
            assert!(!is_valid_map_name(bad), "{bad}");
        }
    }

    #[test]
    fn resolve_env_splits_map_names_with_known_names_first() {
        let none = |_: &str| Vec::<String>::new();
        let r = resolve_env("AWMAN_AGENTS_STUCK_TIMEOUT", &none).unwrap();
        assert_eq!(r.path, "agents.stuckTimeout");
        let r = resolve_env("AWMAN_REMOTE_TARGETS_PROD_API_KEY", &none).unwrap();
        assert_eq!(r.path, "remote.targets.prod.apiKey");
        let known = |section: &str| {
            if section == "workServers" {
                vec!["in-fact".to_string()]
            } else {
                vec![]
            }
        };
        let r = resolve_env("AWMAN_WORK_SERVERS_IN_FACT_URL", &known).unwrap();
        assert_eq!(r.path, "workServers.in-fact.url");
        let r = resolve_env("AWMAN_WORK_SERVERS_IN_FACT_URL", &none).unwrap();
        assert_eq!(r.path, "workServers.in_fact.url");
        assert!(resolve_env("AWMAN_HOME", &none).is_none());
        assert!(resolve_env("AWMAN_RUNTIME_BUILTIN_IMAGES_X", &none).is_none());
    }

    #[test]
    fn parse_input_by_kind() {
        let k = |p: &str| resolve(p).unwrap().key;
        assert_eq!(
            parse_input(k("daemon.verboseSetup"), "true").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            parse_input(k("daemon.maxConcurrentJobs"), "3").unwrap(),
            Value::from(3)
        );
        assert!(parse_input(k("daemon.maxConcurrentJobs"), "0").is_err());
        assert_eq!(
            parse_input(k("overlays"), "ssh(), env(A)").unwrap(),
            serde_json::json!(["ssh()", "env(A)"])
        );
        assert!(parse_input(k("agents.stuckTimeout"), "30").is_err());
        assert_eq!(
            parse_input(k("agents.stuckTimeout"), "45s").unwrap(),
            Value::from("45s")
        );
        assert!(parse_input(k("runtime.kind"), "docker-sbx-experimental").is_err());
        assert_eq!(
            parse_input(k("workServers.a.labels"), "zone=eu, gpu=yes").unwrap(),
            serde_json::json!({"zone": "eu", "gpu": "yes"})
        );
        assert!(matches!(
            parse_input(k("runtime.builtin.imageSource"), "{}"),
            Err(KeyError::ObjectNotSettable { .. })
        ));
        assert_eq!(
            parse_input(k("agents.default"), "null").unwrap(),
            Value::Null
        );
    }

    #[test]
    fn siblings_list_children_and_map_levels() {
        let top = siblings("");
        assert!(
            top.contains(&"agents") && top.contains(&"workServers"),
            "{top:?}"
        );
        assert_eq!(siblings("remote"), ["default", "targets"]);
        assert_eq!(siblings("workServers"), ["<name>"]);
        assert!(siblings("workServers.x").contains(&"url"));
    }

    #[test]
    fn moved_hint_uses_the_longest_prefix() {
        assert_eq!(
            moved_hint("agentStuckTimeout"),
            "; `agentStuckTimeout` moved to `agents.stuckTimeout` (\"30s\")"
        );
        assert_eq!(
            moved_hint("builtin.vcpus"),
            "; `builtin` moved to `runtime.builtin`"
        );
        assert_eq!(
            moved_hint("builtin.memoryMib"),
            "; `builtin.memoryMib` moved to `runtime.builtin.memory` (\"4GiB\")"
        );
        assert_eq!(moved_hint("nothing"), "");
    }

    #[test]
    fn unknown_key_message_lists_siblings_and_hint() {
        let doc = serde_json::json!({"agentStuckTimeout": 30});
        let err = validate_document(&doc, FileKind::Repo).unwrap_err();
        let text = err
            .in_file(std::path::Path::new("/repo/.awman/config.json"))
            .to_string();
        assert!(text.starts_with("unknown config key `agentStuckTimeout` in /repo/.awman/config.json; valid keys here: agents, daemon,"), "{text}");
        assert!(
            text.ends_with("`agentStuckTimeout` moved to `agents.stuckTimeout` (\"30s\")"),
            "{text}"
        );
    }

    #[test]
    fn scope_is_enforced_per_file_kind() {
        let global_only = serde_json::json!({"runtime": {"kind": "docker"}});
        assert!(validate_document(&global_only, FileKind::Global).is_ok());
        let err = validate_document(&global_only, FileKind::Repo).unwrap_err();
        assert!(matches!(err, KeyError::Scope { .. }), "{err}");
        assert!(err.to_string().contains("runtime.kind"), "{err}");
        assert!(err.to_string().contains("global config file"), "{err}");

        let daemon = serde_json::json!({"daemon": {"maxConcurrentJobs": 3}});
        assert!(matches!(
            validate_document(&daemon, FileKind::Task),
            Err(KeyError::Scope { .. })
        ));
        // Task-scope keys are allowed in repo files (task scope includes repo).
        let dynamic = serde_json::json!({"workflows": {"dynamic": {"guidance": ["x"]}}});
        assert!(validate_document(&dynamic, FileKind::Repo).is_ok());
        assert!(validate_document(&dynamic, FileKind::Task).is_ok());
        // Repo-scope keys are not allowed in task files.
        let repo = serde_json::json!({"agents": {"auth": {"mode": "none"}}});
        assert!(matches!(
            validate_document(&repo, FileKind::Task),
            Err(KeyError::Scope { .. })
        ));
    }

    #[test]
    fn map_names_are_validated_and_dash_underscore_duplicates_rejected() {
        let bad = serde_json::json!({"remote": {"targets": {"Prod": {"url": "https://x"}}}});
        assert!(matches!(
            validate_document(&bad, FileKind::Global),
            Err(KeyError::Name { .. })
        ));
        let bad_server = serde_json::json!({"workServers": {"Bad Name": {"url": "https://x"}}});
        assert!(matches!(
            validate_document(&bad_server, FileKind::Global),
            Err(KeyError::Name { .. })
        ));
        let dup =
            serde_json::json!({"workServers": {"in-fact": {"url": "u"}, "in_fact": {"url": "u"}}});
        let err = validate_document(&dup, FileKind::Global).unwrap_err();
        assert!(matches!(err, KeyError::DuplicateName { .. }), "{err}");
        let dup = serde_json::json!({"agents": {"flags": {"my-agent": [], "my_agent": []}}});
        assert!(matches!(
            validate_document(&dup, FileKind::Repo),
            Err(KeyError::DuplicateName { .. })
        ));
        // Image tags and registry hosts are free-form object-map keys.
        let images = serde_json::json!({"runtime": {"builtin": {
            "images": {"awman-x-claude:latest": {}},
            "registries": {"ghcr.io": {}}
        }}});
        assert!(validate_document(&images, FileKind::Repo).is_ok());
    }

    #[test]
    fn values_are_type_checked_with_the_concrete_path() {
        let doc = serde_json::json!({"workServers": {"a": {"maxConcurrentJobs": 0}}});
        let err = validate_document(&doc, FileKind::Global)
            .unwrap_err()
            .to_string();
        assert!(err.contains("workServers.a.maxConcurrentJobs"), "{err}");
        let doc = serde_json::json!({"agents": "claude"});
        assert!(validate_document(&doc, FileKind::Repo).is_err());
    }

    #[test]
    fn sensitive_values_never_appear_in_validation_errors() {
        for bad in [
            serde_json::json!(["FAKE_REVIEW_SECRET_123456"]),
            serde_json::json!({"k": "FAKE_REVIEW_SECRET_123456"}),
            serde_json::json!(12345678),
        ] {
            let doc = serde_json::json!({"remote": {"targets": {"prod": {
                "url": "https://example.com", "apiKey": bad}}}});
            let err = validate_document(&doc, FileKind::Global)
                .unwrap_err()
                .to_string();
            assert!(err.contains("remote.targets.prod.apiKey"), "{err}");
            assert!(!err.contains("FAKE_REVIEW_SECRET_123456"), "{err}");
            assert!(!err.contains("12345678"), "{err}");
        }
    }

    #[test]
    fn docs_table_has_one_line_per_row() {
        let table = render_docs_table();
        assert_eq!(table.lines().count(), CONFIG_KEYS.len() + 2);
        assert!(table.contains("`AWMAN_WORK_SERVERS_<NAME>_URL`"));
    }
}

#[cfg(test)]
mod dotted_member_tests {
    use super::*;
    use crate::data::config::config_json::{config_field_value, remove_config_field};

    #[test]
    fn dotted_free_form_map_members_are_one_segment() {
        assert_eq!(
            path_segments("runtime.builtin.registries.ghcr.io"),
            ["runtime", "builtin", "registries", "ghcr.io"]
        );
        assert_eq!(
            path_segments("tui.integrations.rex.enabled"),
            ["tui", "integrations", "rex", "enabled"]
        );
        assert_eq!(
            resolve("tui.integrations.rex.enabled").unwrap().names,
            ["rex"]
        );
        let resolved = resolve("runtime.builtin.registries.ghcr.io").unwrap();
        assert_eq!(resolved.names, ["ghcr.io"]);
        let mut doc = serde_json::json!({"runtime": {"builtin": {"registries": {"ghcr.io": {"insecure": false}}}}});
        assert_eq!(
            config_field_value(&doc, "runtime.builtin.registries.ghcr.io").as_deref(),
            Some(r#"{"insecure":false}"#)
        );
        remove_config_field(&mut doc, "runtime.builtin.registries.ghcr.io");
        assert_eq!(
            doc["runtime"]["builtin"]["registries"],
            serde_json::json!({})
        );

        let label = resolve("workServers.prod.labels.team.example/owner").unwrap();
        assert_eq!(label.names, ["prod", "team.example/owner"]);
        let labels = serde_json::json!({"workServers": {"prod": {"url": "https://example.com", "labels": {"team.example/owner": "build"}}}});
        assert!(validate_document(&labels, FileKind::Global).is_ok());
        assert!(
            super::super::work_servers::validate_configured_label("awman/os")
                .unwrap_err()
                .contains("reserved awman/ namespace")
        );
    }
}

#[cfg(test)]
mod daemon_config_tests {
    use super::*;

    fn daemon_key(path: &str) -> &'static ConfigKey {
        CONFIG_KEYS.iter().find(|key| key.path == path).unwrap()
    }

    #[test]
    fn daemon_key_live_flags_match_reload_contract() {
        for key in CONFIG_KEYS
            .iter()
            .filter(|key| key.path.starts_with("daemon.features."))
        {
            assert!(key.live, "{} should be live", key.path);
            assert_eq!(key.scope, KeyScope::Global, "{}", key.path);
        }
        for path in [
            "daemon.maxConcurrentJobs",
            "daemon.terminal.remoteInput",
            "daemon.network.enabled",
            "daemon.network.tlsSans",
            "daemon.network.features",
            "daemon.features.squad.enabled",
            "daemon.features.worker.enabled",
            "daemon.features.engine.enabled",
        ] {
            assert!(is_live(path), "{path} should be live");
            assert_eq!(daemon_key(path).scope, KeyScope::Global, "{path}");
        }
        assert!(!is_live("daemon.network.listen"));
        assert_eq!(daemon_key("daemon.network.listen").scope, KeyScope::Global);
        assert_eq!(daemon_key("daemon.maxConcurrentJobs").default, "4");
    }

    #[test]
    fn worker_config_rows_match_scope_live_and_defaults() {
        let worker_rows = CONFIG_KEYS
            .iter()
            .filter(|key| key.path.starts_with("daemon.features.worker."));
        for key in worker_rows {
            assert_eq!(key.scope, KeyScope::Global, "{}", key.path);
            assert!(key.live, "{} should apply live", key.path);
        }
        for key in CONFIG_KEYS
            .iter()
            .filter(|key| key.path.starts_with("workServers.*."))
        {
            assert_eq!(key.scope, KeyScope::Global, "{}", key.path);
            assert!(key.live, "{} should apply live", key.path);
        }
        assert_eq!(
            daemon_key("daemon.features.worker.push.interaction").default,
            "\"headless\""
        );
        assert_eq!(
            daemon_key("daemon.features.worker.pull.interaction").default,
            "\"interactive\""
        );
        assert_eq!(
            daemon_key("daemon.features.worker.pull.workRoot").default,
            "null"
        );
        assert_eq!(daemon_key("tui.worker.enabled").default, "false");
        assert!(parse_input(daemon_key("tui.worker.enabled"), "true").is_ok());
    }

    #[test]
    fn daemon_network_values_validate_and_invalid_sans_are_not_accepted() {
        for listen in ["127.0.0.1:9876", "[::]:9876", "[2001:db8::1]:443"] {
            assert!(validate_value(
                daemon_key("daemon.network.listen"),
                &Value::String(listen.into())
            )
            .is_ok());
        }
        for listen in ["127.0.0.1", "[::]"] {
            assert!(validate_value(
                daemon_key("daemon.network.listen"),
                &Value::String(listen.into())
            )
            .is_err());
        }
        let valid = serde_json::json!({"daemon": {"network": {"tlsSans": ["api.example.org", "192.0.2.1", "2001:db8::1"]}}});
        assert!(validate_document(&valid, FileKind::Global).is_ok());
        let invalid = serde_json::json!({"daemon": {"network": {"tlsSans": ["bad_name.example"]}}});
        let before = invalid.clone();
        assert!(validate_document(&invalid, FileKind::Global).is_err());
        assert_eq!(
            invalid, before,
            "failed validation must not alter persisted input"
        );
    }

    #[test]
    fn removed_api_squad_and_workers_keys_are_unknown_with_move_hints() {
        for (path, expected_hint) in [
            ("api.bind", "daemon.network.listen"),
            ("api.oldOption", "daemon.features.worker.push"),
            ("squad.oldOption", "daemon.features.squad"),
            ("workers", "daemon.maxConcurrentJobs"),
        ] {
            let err = resolve(path).unwrap_err();
            assert!(matches!(err, KeyError::Unknown { .. }), "{path}: {err}");
            assert!(err.to_string().contains(expected_hint), "{path}: {err}");
        }
        let config = serde_json::json!({"api": {"bind": "0.0.0.0:9876"}});
        let err = validate_document(&config, FileKind::Global).unwrap_err();
        assert!(matches!(err, KeyError::Unknown { .. }), "{err}");
        assert!(err.to_string().contains("daemon.network.listen"), "{err}");
    }
}
