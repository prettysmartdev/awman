//! `ConfigFieldSpec` — one row per config field.
//!
//! Before WI 0114 F-51 a config field's properties were spread over seven
//! string-keyed lookups in `commands/config.rs`: a `(name, scope)` table, a
//! `match name` for the value kind, a `READ_ONLY_FIELDS` slice, a
//! `field_writability` derivation, a `match name` for the format hint, and a
//! `SENSITIVE_FIELDS` slice. Adding a field meant editing up to six of them,
//! and nothing made it obvious which.
//!
//! Layer 0 because the fields *are* the config schema: what `RepoConfig` and
//! `GlobalConfig` accept, which of the two a value may be written to, and what
//! shape a value has.

/// Which config file a field may be written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldScope {
    /// May only be written to global config.
    GlobalOnly,
    /// May only be written to repo config.
    RepoOnly,
    /// May be written to either global or repo config.
    Both,
}

/// What kind of value a field accepts.
///
/// Lets a renderer format the value cell and lets `set` reject bad input
/// early. Distinct from a field's *shape* (scalar, map member, array element),
/// which is `ConfigFieldShape` in Layer 2 — that is about what a frontend may
/// do with a row, not about what the value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldKind {
    Bool,
    Number,
    /// Fixed enum (e.g. agent name); the `set` validator rejects values
    /// outside the documented set.
    Enum,
    String,
}

/// One config field, whole.
#[derive(Debug, Clone, Copy)]
pub struct ConfigFieldSpec {
    /// Dotted name, as `awman config get|set` takes it.
    pub name: &'static str,
    pub scope: FieldScope,
    pub kind: ConfigFieldKind,
    /// Computed by awman itself and not settable by the user. Surfaced with
    /// `(read-only)` in the table.
    pub read_only: bool,
    /// Holds a credential: shown masked, never in full.
    pub sensitive: bool,
    /// Human-readable format hint shown while editing. `None` for fields
    /// whose shape is evident from the current value.
    pub hint: Option<&'static str>,
}

impl ConfigFieldSpec {
    /// Whether this field may be written to `(global, repo)`. A read-only
    /// field is writable in neither.
    pub fn writability(&self) -> (bool, bool) {
        if self.read_only {
            return (false, false);
        }
        match self.scope {
            FieldScope::GlobalOnly => (true, false),
            FieldScope::RepoOnly => (false, true),
            FieldScope::Both => (true, true),
        }
    }
}

/// Every config field `awman config` accepts.
///
/// Order is display order in `awman config show`.
pub const CONFIG_FIELDS: &[ConfigFieldSpec] = &[
    f(
        "workflows.prerequisites.preflight",
        FieldScope::Both,
        ConfigFieldKind::Enum,
        Some("warn | off"),
    ),
    f(
        "workflows.prerequisites.unresolved",
        FieldScope::Both,
        ConfigFieldKind::Enum,
        Some("warn | fail"),
    ),
    f(
        "workflows.prerequisites.gitIdentity.use",
        FieldScope::Both,
        ConfigFieldKind::Enum,
        Some("ask | allow | deny"),
    ),
    f(
        "workflows.prerequisites.gitIdentity.name",
        FieldScope::Both,
        ConfigFieldKind::String,
        None,
    ),
    f(
        "workflows.prerequisites.gitIdentity.email",
        FieldScope::Both,
        ConfigFieldKind::String,
        None,
    ),
    f(
        "workflows.prerequisites.ssh.use",
        FieldScope::Both,
        ConfigFieldKind::Enum,
        Some("ask | allow | deny"),
    ),
    f(
        "workflows.prerequisites.ssh.key",
        FieldScope::Both,
        ConfigFieldKind::String,
        Some("Path under ~/.ssh"),
    ),
    f(
        "workflows.prerequisites.githubToken.use",
        FieldScope::Both,
        ConfigFieldKind::Enum,
        Some("ask | allow | deny"),
    ),
    f(
        "workflows.prerequisites.githubToken.envVars",
        FieldScope::Both,
        ConfigFieldKind::String,
        LIST_HINT,
    ),
    f(
        "workflows.prerequisites.githubToken.verify",
        FieldScope::Both,
        ConfigFieldKind::Bool,
        None,
    ),
    f(
        "workflows.prerequisites.githubToken.acquire",
        FieldScope::GlobalOnly,
        ConfigFieldKind::String,
        Some(r#"JSON object: {"argv":[...],"timeoutSecs":30,"refresh":"start"}"#),
    ),
    f(
        "workflows.prerequisites.allowApiClientGrants",
        FieldScope::GlobalOnly,
        ConfigFieldKind::Bool,
        None,
    ),
    f("agent", FieldScope::Both, ConfigFieldKind::Enum, AGENT_HINT),
    ConfigFieldSpec {
        name: "auto_agent_auth_accepted",
        scope: FieldScope::GlobalOnly,
        kind: ConfigFieldKind::Bool,
        read_only: true,
        sensitive: false,
        hint: Some("true or false"),
    },
    f(
        "terminal_scrollback_lines",
        FieldScope::Both,
        ConfigFieldKind::Number,
        Some("positive integer"),
    ),
    f(
        "yoloDisallowedTools",
        FieldScope::Both,
        ConfigFieldKind::String,
        LIST_HINT,
    ),
    f(
        "workItems",
        FieldScope::RepoOnly,
        ConfigFieldKind::String,
        None,
    ),
    f(
        "overlays",
        FieldScope::Both,
        ConfigFieldKind::String,
        LIST_HINT,
    ),
    f(
        "agentStuckTimeout",
        FieldScope::Both,
        ConfigFieldKind::Number,
        Some("positive integer"),
    ),
    f(
        "maxConcurrentAgents",
        FieldScope::Both,
        ConfigFieldKind::Number,
        Some("integer >= 1 (unset = unlimited)"),
    ),
    f(
        "runtime",
        FieldScope::GlobalOnly,
        ConfigFieldKind::String,
        None,
    ),
    f(
        "builtin.stateDir",
        FieldScope::Both,
        ConfigFieldKind::String,
        Some("absolute private state path"),
    ),
    f(
        "builtin.vcpus",
        FieldScope::Both,
        ConfigFieldKind::Number,
        Some("whole vCPUs, 1-255"),
    ),
    f(
        "builtin.memoryMib",
        FieldScope::Both,
        ConfigFieldKind::Number,
        Some("memory in MiB, at least 128"),
    ),
    f(
        "builtin.rootDiskMib",
        FieldScope::Both,
        ConfigFieldKind::Number,
        Some("writable root disk in MiB, at least 256 (default 30720)"),
    ),
    f(
        "builtin.mountQuotaMib",
        FieldScope::Both,
        ConfigFieldKind::Number,
        Some("MiB the agent may add to each mount, at least 1 (default 30720)"),
    ),
    f(
        "builtin.builder.enabled",
        FieldScope::Both,
        ConfigFieldKind::Bool,
        Some("true or false (default false): build missing images in the source's own store"),
    ),
    ConfigFieldSpec {
        name: "builtin.imageSource",
        scope: FieldScope::Both,
        kind: ConfigFieldKind::String,
        read_only: true,
        sensitive: false,
        hint: Some("edit the typed source object in config JSON"),
    },
    ConfigFieldSpec {
        name: "builtin.images",
        scope: FieldScope::Both,
        kind: ConfigFieldKind::String,
        read_only: true,
        sensitive: false,
        hint: Some("edit per-image sources in config JSON"),
    },
    ConfigFieldSpec {
        name: "builtin.registries",
        scope: FieldScope::Both,
        kind: ConfigFieldKind::String,
        read_only: true,
        sensitive: false,
        hint: Some("edit registry settings in config JSON"),
    },
    ConfigFieldSpec {
        name: "builtin.network",
        scope: FieldScope::Both,
        kind: ConfigFieldKind::String,
        read_only: true,
        sensitive: false,
        hint: Some("edit the network policy object in config JSON"),
    },
    f(
        "default_agent",
        FieldScope::GlobalOnly,
        ConfigFieldKind::Enum,
        AGENT_HINT,
    ),
    f("api", FieldScope::GlobalOnly, ConfigFieldKind::String, None),
    f("remote", FieldScope::Both, ConfigFieldKind::String, None),
    // Dot-notation nested fields
    f(
        "work_items.dir",
        FieldScope::RepoOnly,
        ConfigFieldKind::String,
        None,
    ),
    f(
        "work_items.template",
        FieldScope::RepoOnly,
        ConfigFieldKind::String,
        None,
    ),
    f(
        "api.workDirs",
        FieldScope::GlobalOnly,
        ConfigFieldKind::String,
        LIST_HINT,
    ),
    f(
        "api.workDirRoots",
        FieldScope::GlobalOnly,
        ConfigFieldKind::String,
        LIST_HINT,
    ),
    f(
        "api.port",
        FieldScope::GlobalOnly,
        ConfigFieldKind::Number,
        Some("positive integer"),
    ),
    f(
        "api.background",
        FieldScope::GlobalOnly,
        ConfigFieldKind::Bool,
        Some("true or false"),
    ),
    f(
        "remote.defaultAddr",
        FieldScope::Both,
        ConfigFieldKind::String,
        None,
    ),
    ConfigFieldSpec {
        name: "remote.defaultAPIKey",
        scope: FieldScope::Both,
        kind: ConfigFieldKind::String,
        read_only: false,
        sensitive: true,
        hint: None,
    },
    f(
        "worktree.onDirtyMergeTarget",
        FieldScope::Both,
        ConfigFieldKind::Enum,
        Some("ask | commit | stash | fail"),
    ),
    f(
        "worktree.mergeFix.onFailure",
        FieldScope::Both,
        ConfigFieldKind::Enum,
        Some("ask | agent | keep"),
    ),
    f(
        "worktree.mergeFix.promptTemplate",
        FieldScope::Both,
        ConfigFieldKind::String,
        Some("path relative to the git root"),
    ),
    // Dynamic-workflow config (WI-0095), repo-only.
    f(
        "dynamicWorkflows.defaultLeader",
        FieldScope::RepoOnly,
        ConfigFieldKind::String,
        Some("agent::model (e.g. claude::claude-opus-4-8)"),
    ),
    f(
        "dynamicWorkflows.maxConcurrentSteps",
        FieldScope::RepoOnly,
        ConfigFieldKind::Number,
        Some("integer >= 1"),
    ),
    ConfigFieldSpec {
        name: "dynamicWorkflows.agentsToModels",
        scope: FieldScope::RepoOnly,
        kind: ConfigFieldKind::String,
        // The map's own row is a header; the per-agent rows beneath it are
        // editable.
        read_only: true,
        sensitive: false,
        hint: Some("press Ctrl+N to add an agent; edit per-agent rows inline"),
    },
    ConfigFieldSpec {
        name: "dynamicWorkflows.guidance",
        scope: FieldScope::RepoOnly,
        kind: ConfigFieldKind::String,
        read_only: true,
        sensitive: false,
        hint: Some(
            "press Ctrl+N to add a guidance entry; edit per-entry rows inline; save an empty \
             value to remove",
        ),
    },
];

/// The hint shared by the two agent-name fields. Filled in at use time with
/// the catalogue's agent list, which is why it is not a plain string here.
const AGENT_HINT: Option<&'static str> = Some("one of: {agents}");
const LIST_HINT: Option<&'static str> = Some("comma-separated list");

/// The common case: not read-only, not sensitive.
const fn f(
    name: &'static str,
    scope: FieldScope,
    kind: ConfigFieldKind,
    hint: Option<&'static str>,
) -> ConfigFieldSpec {
    ConfigFieldSpec {
        name,
        scope,
        kind,
        read_only: false,
        sensitive: false,
        hint,
    }
}

/// The spec for `name`, if it is a known field.
pub fn field_spec(name: &str) -> Option<&'static ConfigFieldSpec> {
    CONFIG_FIELDS.iter().find(|spec| spec.name == name)
}

/// The value a field takes when neither scope sets it, for fields whose
/// typed effective view has a fixed default the raw JSON does not show.
/// `config show` / `config get` use it for the effective column only, so the
/// global and repo cells still show that nothing is set.
pub fn implicit_default(name: &str) -> Option<&'static str> {
    match name {
        crate::data::config::builtin_runtime::BUILDER_ENABLED_FIELD => Some("false"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_name_is_unique() {
        let mut names: Vec<&str> = CONFIG_FIELDS.iter().map(|f| f.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate field name in CONFIG_FIELDS");
    }

    #[test]
    fn api_work_dir_roots_is_a_global_only_list_like_work_dirs() {
        let find = |name: &str| CONFIG_FIELDS.iter().find(|f| f.name == name).unwrap();
        let roots = find("api.workDirRoots");
        let dirs = find("api.workDirs");
        assert!(matches!(roots.scope, FieldScope::GlobalOnly));
        assert_eq!(roots.writability(), (true, false));
        assert_eq!(roots.writability(), dirs.writability());
        assert_eq!(roots.hint, dirs.hint);
    }

    #[test]
    fn a_read_only_field_is_writable_in_neither_scope() {
        for spec in CONFIG_FIELDS.iter().filter(|f| f.read_only) {
            assert_eq!(
                spec.writability(),
                (false, false),
                "{} is read-only",
                spec.name
            );
        }
    }

    #[test]
    fn writability_follows_scope() {
        for spec in CONFIG_FIELDS.iter().filter(|f| !f.read_only) {
            let expected = match spec.scope {
                FieldScope::GlobalOnly => (true, false),
                FieldScope::RepoOnly => (false, true),
                FieldScope::Both => (true, true),
            };
            assert_eq!(spec.writability(), expected, "{}", spec.name);
        }
    }

    /// The one credential field. A new sensitive field must be added here
    /// deliberately, because forgetting `sensitive` prints a key in full.
    #[test]
    fn only_the_remote_api_key_is_sensitive() {
        let sensitive: Vec<&str> = CONFIG_FIELDS
            .iter()
            .filter(|f| f.sensitive)
            .map(|f| f.name)
            .collect();
        assert_eq!(sensitive, vec!["remote.defaultAPIKey"]);
    }

    #[test]
    fn builder_enabled_is_a_boolean_editable_in_both_scopes_with_false_default() {
        let spec = field_spec("builtin.builder.enabled").unwrap();
        assert_eq!(spec.scope, FieldScope::Both);
        assert_eq!(spec.kind, ConfigFieldKind::Bool);
        assert!(!spec.read_only);
        assert_eq!(implicit_default(spec.name), Some("false"));
        assert!(spec.hint.unwrap().contains("source's own store"));
    }
}
