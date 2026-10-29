//! `ConfigCommand` — view and edit global / repo configuration.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::{BuildContext, Engines};
use crate::command::error::CommandError;
use crate::data::config::config_json::{apply_config_field, config_field_value};
pub use crate::data::config::fields::ConfigRowShape;
use crate::data::config::fields::{
    field_spec, implicit_default, json_path, resolve_field_name, writability, ConfigFieldSpec,
    FieldScope, ResolvedFieldName, CONFIG_FIELDS, LIST_ENTRY_HINT, MAP_ENTRY_HINT,
};
use crate::data::config::migrations::{
    ConfigMigrationChange, ConfigMigrationScope, GlobalConfigMigration, RepoConfigMigration,
};
use crate::data::config::ResolvedDynamicWorkflows;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::prompt::Prompt;

/// Field names that were removed in WI-0082 (overlay unification). Naming any
/// of these in `awman config get|set` returns a guidance error instead of the
/// generic "unknown field" suggestion list.
const REMOVED_CONFIG_FIELDS: &[(&str, &str)] = &[(
    "envPassthrough",
    "the 'envPassthrough' field was removed; express env passthrough as overlay entries \
     instead (e.g. `awman config set overlays \"env(VAR_NAME)\"` or add `\"env(VAR_NAME)\"` \
     to the `overlays` array in `.awman/config.json`). See `docs/08-overlays.md`.",
)];

/// Shown when `config get|set` was given a legacy `dynamicWorkflows.*` name.
const DYNAMIC_WORKFLOWS_ALIAS_NOTICE: &str = "`dynamicWorkflows.*` is now `workflows.dynamic.*`";

use crate::data::config::repo::MOVED_LEGACY_DYNAMIC_WORKFLOWS_NOTICE;

fn removed_field_message(name: &str) -> Option<&'static str> {
    REMOVED_CONFIG_FIELDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, msg)| *msg)
}

/// Flat list of all valid canonical field names (for suggestions). Legacy
/// aliases are accepted as input but never suggested.
fn valid_field_names() -> Vec<&'static str> {
    CONFIG_FIELDS.iter().map(|spec| spec.name).collect()
}

/// Resolve a field name against the registry: a canonical name, a member of
/// a map or list header (`<child_prefix><member>`), or a legacy alias of
/// either. A list member must be a bare non-negative index, which keeps the
/// numeric-index dot-path handling narrow to list rows rather than a general
/// array facility.
fn resolve_config_field(name: &str) -> Option<ResolvedFieldName> {
    let resolved = resolve_field_name(name)?;
    let bad_index = resolved.shape == ConfigRowShape::ListEntry
        && resolved
            .member
            .as_deref()
            .is_some_and(|m| m.parse::<usize>().is_err());
    (!bad_index).then_some(resolved)
}

/// Lexical validation for a map key. The registry's one map
/// (`workflows.dynamic.agentsToModels`) is keyed by agent name, so the rule
/// is `AgentName`'s, in Layer 0. This used to restate it, and so did the
/// TUI's dialog router; three copies of one rule, free to drift (WI 0114 F-20).
fn validate_map_entry_key(key: &str) -> Result<(), String> {
    crate::data::session::AgentName::new(key)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Resolve a `config get|set` input, or the user-facing error for a removed
/// or unknown name. Suggestions are computed over canonical names only.
fn resolve_input_field(name: &str) -> Result<ResolvedFieldName, CommandError> {
    if let Some(msg) = removed_field_message(name) {
        return Err(CommandError::Other(msg.to_string()));
    }
    resolve_config_field(name).ok_or_else(|| {
        let suggestions = field_suggestions(name, &valid_field_names());
        CommandError::UnknownConfigField {
            name: name.to_string(),
            suggestions: if suggestions.is_empty() {
                "(none)".to_string()
            } else {
                suggestions.join(", ")
            },
        }
    })
}

/// The agents a `config set agent` write may name for its scope: a repo
/// write accepts first-party, repo-custom and global-custom names; a global
/// write accepts first-party and global-custom names (a repo definition is
/// unknown in every other repo).
fn agent_listing_for_scope(
    session: &crate::data::session::Session,
    global: bool,
) -> crate::engine::agent::registry::AgentListing {
    let root = (!global).then(|| session.definition_root());
    crate::engine::agent::registry::AgentRegistry::list(session.env(), root)
}

fn is_agent_field(field: &str) -> bool {
    matches!(field, "agent" | "default_agent")
}

/// Reject an `agent` value the scope's listing does not contain.
fn check_agent_known(
    value: &str,
    listing: &crate::engine::agent::registry::AgentListing,
    global: bool,
) -> Result<(), String> {
    if listing.contains(value) {
        return Ok(());
    }
    let scope = if global { "global" } else { "repo" };
    Err(format!(
        "'{value}' is not a known agent for {scope} config; valid agents: {}; define one with: \
         awman new agent {value}{}",
        listing.names().join(", "),
        if global { " --global" } else { "" }
    ))
}

/// Validate and coerce a string value into the appropriate JSON type for the
/// given field. Returns the coerced `serde_json::Value` or a user-facing error.
///
/// Map/list members and headers are recognised by their registry shape, not
/// by name; a legacy alias coerces exactly like its canonical name.
fn validate_and_coerce(field: &str, value: &str) -> Result<serde_json::Value, String> {
    let Some(resolved) = resolve_config_field(field) else {
        return validate_and_coerce_scalar(field, value);
    };
    match resolved.shape {
        ConfigRowShape::MapEntry => {
            validate_map_entry_key(resolved.member.as_deref().unwrap_or_default())?;
            let items: Vec<&str> = value
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            // A member cannot be mapped to zero models (config validation
            // rejects empty lists), so an empty value means "remove this entry".
            if items.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            Ok(serde_json::Value::Array(
                items
                    .iter()
                    .map(|s| serde_json::Value::String(s.to_string()))
                    .collect(),
            ))
        }
        ConfigRowShape::ListEntry => {
            // A list entry is a single free-form instruction (it may contain
            // commas, markdown, etc.), so it is NOT comma-split. An empty or
            // whitespace-only value coerces to Null, which the config layer
            // treats as "remove this array element".
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            if trimmed.chars().count() > crate::data::config::repo::GUIDANCE_MAX_ENTRY_LEN {
                return Err(format!(
                    "guidance entry is {} characters; the maximum is {}",
                    trimmed.chars().count(),
                    crate::data::config::repo::GUIDANCE_MAX_ENTRY_LEN
                ));
            }
            Ok(serde_json::Value::String(trimmed.to_string()))
        }
        // A map or list value can't be expressed as a `config set` string;
        // without this rejection the coerced string would fail config
        // deserialization and the set would silently write nothing. Point
        // users at the per-member syntax.
        ConfigRowShape::MapHeader { child_prefix } => Err(format!(
            "{} is a map; set one agent at a time, e.g. `awman config set {child_prefix}claude \
             \"model-a, model-b\"`",
            resolved.spec.name
        )),
        ConfigRowShape::ListHeader { child_prefix } => Err(format!(
            "{} is a list; set one entry at a time by index, e.g. `awman config set \
             {child_prefix}0 \"never spawn more than two agents in parallel\"`",
            resolved.spec.name
        )),
        ConfigRowShape::Scalar => validate_and_coerce_scalar(&resolved.canonical, value),
    }
}

/// [`validate_and_coerce`] for a single-valued field, by canonical name.
fn validate_and_coerce_scalar(field: &str, value: &str) -> Result<serde_json::Value, String> {
    if field == "runtime" {
        use crate::data::config::RuntimeSelection;
        return match RuntimeSelection::parse(Some(value)) {
            Ok(_) => Ok(serde_json::Value::String(value.trim().to_string())),
            Err(unknown) => Err(format!(
                "'{}' is not a runtime; valid values: {}{}",
                unknown.0,
                RuntimeSelection::valid_values(),
                RuntimeSelection::hint_for(&unknown.0)
                    .map(|hint| format!(" ({hint})"))
                    .unwrap_or_default()
            )),
        };
    }
    if field == crate::data::config::builtin_runtime::BUILDER_ENABLED_FIELD {
        // The generic bool fallback below also accepts exactly "true"/"false"
        // and would coerce the same way, but routes through here for the
        // field-naming error message `parse_enabled` gives on anything else.
        return crate::data::config::builtin_runtime::BuiltinBuilderConfig::parse_enabled(value)
            .map(serde_json::Value::Bool);
    }
    match field {
        "worktree.onDirtyMergeTarget" => value
            .parse::<crate::data::config::OnDirtyMergeTarget>()
            .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null)),
        "worktree.mergeFix.onFailure" => value
            .parse::<crate::data::config::OnMergeFailure>()
            .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null)),
        "agent" | "default_agent" => {
            // Shape only here; whether the name is defined depends on the
            // scope being written (`check_agent_known`).
            crate::data::session::AgentName::new(value).map_err(|e| e.to_string())?;
            Ok(serde_json::Value::String(value.to_string()))
        }
        "yoloDisallowedTools" | "overlays" | "api.workDirs" | "api.workDirRoots" => {
            // Parse comma-separated into array
            let items: Vec<&str> = value
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            Ok(serde_json::Value::Array(
                items
                    .iter()
                    .map(|s| serde_json::Value::String(s.to_string()))
                    .collect(),
            ))
        }
        "terminal_scrollback_lines" | "agentStuckTimeout" | "api.port" => {
            // Must be a positive integer
            value
                .parse::<u64>()
                .map(|n| serde_json::Value::Number(n.into()))
                .map_err(|_| format!("'{}' is not a valid number", value))
        }
        "workflows.dynamic.maxConcurrentSteps" => {
            // Positive integer; zero would deadlock any workflow (WI-0095).
            let n = value
                .parse::<u64>()
                .map_err(|_| format!("'{}' is not a valid number", value))?;
            if n < 1 {
                return Err("workflows.dynamic.maxConcurrentSteps must be >= 1".to_string());
            }
            Ok(serde_json::Value::Number(n.into()))
        }
        "maxConcurrentAgents" => {
            let n = value
                .parse::<u64>()
                .map_err(|_| format!("'{}' is not a valid number", value))?;
            if n < 1 {
                return Err("maxConcurrentAgents must be >= 1".to_string());
            }
            Ok(serde_json::Value::Number(n.into()))
        }
        "workflows.dynamic.defaultLeader" => {
            validate_default_leader_value(value)?;
            Ok(serde_json::Value::String(value.to_string()))
        }
        _ => {
            // Default: try bool, then number, then string
            if value == "true" {
                Ok(serde_json::Value::Bool(true))
            } else if value == "false" {
                Ok(serde_json::Value::Bool(false))
            } else if let Ok(n) = value.parse::<u64>() {
                Ok(serde_json::Value::Number(n.into()))
            } else {
                Ok(serde_json::Value::String(value.to_string()))
            }
        }
    }
}

/// Validate a `workflows.dynamic.defaultLeader` value for `awman config set`
/// (WI-0095). Enforces the same `agent::model` shape as the Layer 0 config-load
/// validator: exactly two non-empty components, no surrounding whitespace, and
/// an `AgentName`-valid agent component.
fn validate_default_leader_value(value: &str) -> Result<(), String> {
    let parts: Vec<&str> = value.split("::").collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!(
            "'{value}' is not a valid leader; expected agent::model \
             (e.g. claude::claude-opus-4-8)"
        ));
    }
    let (agent, model) = (parts[0], parts[1]);
    if agent != agent.trim() || model != model.trim() {
        return Err(
            "leader agent and model components must not have leading or trailing whitespace"
                .to_string(),
        );
    }
    if agent.len() > 64
        || !agent
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "'{agent}' is not a valid agent name: only ASCII alphanumerics, '-', and '_' are \
             allowed, length 1..=64"
        ));
    }
    Ok(())
}

/// Config-field names within three edits of `input`, nearest first.
///
/// The distance itself is [`crate::data::text::nearest`]; the threshold stays
/// here, because how near a miss is worth offering depends on the vocabulary
/// being searched — a config field list is short and its names are long.
fn field_suggestions<'a>(input: &str, candidates: &[&'a str]) -> Vec<&'a str> {
    crate::data::text::nearest(input, candidates, 3)
}

#[derive(Debug, Clone)]
pub struct ConfigShowFlags {}

#[derive(Debug, Clone)]
pub struct ConfigGetFlags {
    pub field: String,
}

#[derive(Debug, Clone)]
pub struct ConfigSetFlags {
    pub field: String,
    pub value: String,
    pub global: bool,
}

/// `config migrate` flags (WI 0150 §4).
#[derive(Debug, Clone)]
pub struct ConfigMigrateFlags {
    pub yes: bool,
    pub dry_run: bool,
    pub global: bool,
    pub non_interactive: bool,
}

#[derive(Debug, Clone)]
pub enum ConfigSubcommand {
    Show(ConfigShowFlags),
    Get(ConfigGetFlags),
    Set(ConfigSetFlags),
    /// `config migrate` (WI 0150).
    Migrate(ConfigMigrateFlags),
    /// `config prerequisite-grants list|revoke` (WI 0149).
    PrerequisiteGrants(crate::command::commands::workflow_prerequisites::PrerequisiteGrantsAction),
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigShowOutcome {
    pub global: serde_json::Value,
    /// The repo config as awman reads it: a legacy `dynamicWorkflows` block
    /// appears under `workflows.dynamic` (the file itself is untouched).
    pub repo: serde_json::Value,
    /// One row per known field, computed in Layer 2 so the renderer doesn't
    /// need to know which fields exist or which are read-only.
    pub rows: Vec<ConfigFieldRow>,
}

/// Per-field row used by `ConfigShow` rendering.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigFieldRow {
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub effective_value: Option<String>,
    /// What kind of value the field accepts. Lets the renderer (or a
    /// programmatic consumer) format the value cell appropriately and lets
    /// `set` validate input early.
    pub kind: ConfigFieldKind,
    pub read_only: bool,
    /// Whether the value may be written to the global config scope.
    pub global_writable: bool,
    /// Whether the value may be written to the repo config scope.
    pub repo_writable: bool,
    /// Short human-readable format hint shown while editing the value
    /// (e.g. "true or false", "comma-separated list").
    pub value_hint: Option<String>,
    /// What *shape* of config entry this row is, as distinct from what kind
    /// of value it holds: registry data (WI 0150 §5).
    ///
    /// A frontend offering "add an entry" needs to know which row heads a map
    /// and which heads a list, and how to name a new member of it
    /// (`child_prefix`). Frontends branch on this, never on a field name, so
    /// a rename touches no frontend code.
    pub shape: ConfigRowShape,
}

/// Re-exported from Layer 0, where the config schema lives (WI 0114 F-51).
pub use crate::data::config::fields::ConfigFieldKind;

/// Map a known config field name to its `ConfigFieldKind`. Mirrors the
/// schema in `RepoConfig` / `GlobalConfig`. Unknown fields default to
/// `String` (callers should reject them before reaching this function).
fn config_field_kind(name: &str) -> ConfigFieldKind {
    field_spec(name)
        .map(|spec| spec.kind)
        // A map or list member is not in the table; both hold text.
        .unwrap_or(ConfigFieldKind::String)
}

/// Whether a config field is read-only in the show/edit UI. Map and list
/// members are inline-editable; only their header rows and awman-computed
/// fields are read-only.
fn is_read_only_field(name: &str) -> bool {
    field_spec(name).is_some_and(|spec| spec.read_only)
}

/// Per-scope writability for a field, derived from its scope. Read-only
/// fields are writable in neither scope; a member is writable wherever its
/// header's scope allows.
fn field_writability(name: &str) -> (bool, bool) {
    resolve_config_field(name)
        .map(|resolved| writability(&resolved))
        .unwrap_or((false, false))
}

/// Human-readable format hint for a field's value, shown in the TUI config
/// dialog while editing.
fn config_field_hint(name: &str) -> Option<String> {
    match resolve_config_field(name).map(|resolved| resolved.shape) {
        Some(ConfigRowShape::MapEntry) => return Some(MAP_ENTRY_HINT.to_string()),
        Some(ConfigRowShape::ListEntry) => return Some(LIST_ENTRY_HINT.to_string()),
        _ => {}
    }
    let hint = field_spec(name)?.hint?;
    // The two agent fields share one hint whose value set is the registry's,
    // not the table's, so the table carries a placeholder. Custom names
    // depend on the scope being edited, so the hint says where they live;
    // a rejected write lists the scope's exact names.
    let agents = format!(
        "{}, or a custom agent defined in .awman/agents/ or ~/.awman/agents/",
        crate::engine::agent::first_party::FIRST_PARTY_AGENTS.join(", ")
    );
    Some(hint.replace("{agents}", &agents))
}

/// Render a map member's model-list JSON value as a comma-separated string
/// for a display row.
fn render_models_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

fn mask_sensitive(field: &str, value: Option<String>) -> Option<String> {
    if !field_spec(field).is_some_and(|spec| spec.sensitive) {
        return value;
    }
    value.map(|v| {
        if v.len() > 12 {
            format!("{}…{}", &v[..4], &v[v.len() - 4..])
        } else {
            "(set)".to_string()
        }
    })
}

/// The JSON value at a dotted path, if every segment is an object key.
fn json_at<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.').try_fold(root, |value, key| value.get(key))
}

/// The JSON views `config show|get` read: the global config, and the repo
/// config with a legacy `dynamicWorkflows` block adopted into
/// `workflows.dynamic`, so a legacy repo's values appear under the new names
/// and `config get` agrees for either spelling. A view: nothing is written.
fn config_views(session: &crate::data::session::Session) -> (serde_json::Value, serde_json::Value) {
    let global = serde_json::to_value(session.global_config()).unwrap_or(serde_json::Value::Null);
    let mut repo = session.repo_config().clone();
    repo.adopt_legacy_dynamic_workflows();
    let repo = serde_json::to_value(&repo).unwrap_or(serde_json::Value::Null);
    (global, repo)
}

/// The merged dynamic-workflow settings for the given JSON views, as a run
/// resolves them (no flags), or the resolution error's text.
fn resolve_dynamic_view(
    global: &serde_json::Value,
    repo: &serde_json::Value,
) -> Result<ResolvedDynamicWorkflows, String> {
    let repo: crate::data::config::repo::RepoConfig =
        serde_json::from_value(repo.clone()).map_err(|e| e.to_string())?;
    let global: crate::data::config::global::GlobalConfig =
        serde_json::from_value(global.clone()).map_err(|e| e.to_string())?;
    crate::data::config::EffectiveConfig::new(
        crate::data::config::FlagConfig::default(),
        Default::default(),
        repo,
        global,
    )
    .dynamic_workflows()
    .map_err(|e| e.to_string())
}

/// One row per known field, in registry order. Map and list headers are
/// expanded into one row per member (WI-0095, WI-0099), in both scopes
/// (WI 0150), rather than shown as a single unreadable JSON blob.
pub fn collect_config_rows(
    global: &serde_json::Value,
    repo: &serde_json::Value,
) -> Vec<ConfigFieldRow> {
    let dynamic = resolve_dynamic_view(global, repo);
    let mut rows: Vec<ConfigFieldRow> = Vec::new();
    for spec in CONFIG_FIELDS {
        match spec.shape {
            ConfigRowShape::MapHeader { child_prefix } => {
                push_map_rows(&mut rows, spec, child_prefix, global, repo, &dynamic)
            }
            ConfigRowShape::ListHeader { child_prefix } => {
                push_list_rows(&mut rows, spec, child_prefix, global, repo, &dynamic)
            }
            _ => rows.push(scalar_row(spec, global, repo)),
        }
    }
    rows
}

fn scalar_row(
    spec: &ConfigFieldSpec,
    global: &serde_json::Value,
    repo: &serde_json::Value,
) -> ConfigFieldRow {
    let name = spec.name;
    let g = config_field_value(global, &json_path(name));
    let r = config_field_value(repo, &json_path(name));
    // Repo over global, as every `Both` field resolves (the two dynamic
    // scalars included: `EffectiveConfig::dynamic_workflows` layers them the
    // same way when no flag is given).
    let effective = r
        .clone()
        .or_else(|| g.clone())
        .or_else(|| implicit_default(name).map(str::to_string));
    let (global_writable, repo_writable) = spec.writability();
    ConfigFieldRow {
        field: name.to_string(),
        global_value: mask_sensitive(name, g),
        repo_value: mask_sensitive(name, r),
        effective_value: mask_sensitive(name, effective),
        kind: config_field_kind(name),
        read_only: is_read_only_field(name),
        global_writable,
        repo_writable,
        value_hint: config_field_hint(name),
        shape: spec.shape,
    }
}

/// The read-only header row of a map or list. It is always present, so the
/// section is discoverable (and Ctrl+N works) even when empty.
fn header_row(
    spec: &ConfigFieldSpec,
    global_value: String,
    repo_value: String,
    effective_value: String,
) -> ConfigFieldRow {
    let (global_writable, repo_writable) = spec.writability();
    ConfigFieldRow {
        field: spec.name.to_string(),
        global_value: Some(global_value),
        repo_value: Some(repo_value),
        effective_value: Some(effective_value),
        kind: spec.kind,
        read_only: true,
        global_writable,
        repo_writable,
        value_hint: config_field_hint(spec.name),
        shape: spec.shape,
    }
}

fn member_row(
    field: String,
    global_value: Option<String>,
    repo_value: Option<String>,
    effective_value: Option<String>,
) -> ConfigFieldRow {
    let (global_writable, repo_writable) = field_writability(&field);
    let value_hint = config_field_hint(&field);
    let shape = resolve_config_field(&field)
        .map(|resolved| resolved.shape)
        .unwrap_or(ConfigRowShape::Scalar);
    ConfigFieldRow {
        field,
        global_value,
        repo_value,
        effective_value,
        kind: ConfigFieldKind::String,
        read_only: false,
        global_writable,
        repo_writable,
        value_hint,
        shape,
    }
}

/// The map header row, then one row per key in the union of both scopes'
/// maps, sorted for deterministic ordering. A non-empty repo map replaces the
/// global map (WI 0150 §2), so a member's effective value is the one from the
/// map in effect.
fn push_map_rows(
    rows: &mut Vec<ConfigFieldRow>,
    spec: &ConfigFieldSpec,
    child_prefix: &str,
    global: &serde_json::Value,
    repo: &serde_json::Value,
    dynamic: &Result<ResolvedDynamicWorkflows, String>,
) {
    let global_map = json_at(global, spec.name).and_then(|m| m.as_object());
    let repo_map = json_at(repo, spec.name).and_then(|m| m.as_object());
    let summary = |map: Option<&serde_json::Map<String, serde_json::Value>>| match map {
        Some(m) if !m.is_empty() => {
            let n = m.len();
            format!("{n} agent{} mapped", if n == 1 { "" } else { "s" })
        }
        _ => "(none)".to_string(),
    };
    let repo_in_effect = match dynamic {
        Ok(resolved) => resolved
            .agents_to_models
            .as_ref()
            .is_some_and(|map| map.source != crate::data::config::DynamicWorkflowsSource::Global),
        Err(_) => repo_map.is_some_and(|m| !m.is_empty()),
    };
    let effective = match dynamic {
        Err(error) => error.clone(),
        Ok(resolved) => match &resolved.agents_to_models {
            None => "(none)".to_string(),
            Some(_) if repo_in_effect => "repo map (replaces global)".to_string(),
            Some(_) => "global map".to_string(),
        },
    };
    rows.push(header_row(
        spec,
        summary(global_map),
        summary(repo_map),
        effective,
    ));

    let mut keys: Vec<&String> = global_map
        .into_iter()
        .chain(repo_map)
        .flat_map(|m| m.keys())
        .collect();
    keys.sort();
    keys.dedup();
    let in_effect = if repo_in_effect { repo_map } else { global_map };
    for key in keys {
        let value_in = |map: Option<&serde_json::Map<String, serde_json::Value>>| {
            map.and_then(|m| m.get(key)).map(render_models_value)
        };
        rows.push(member_row(
            format!("{child_prefix}{key}"),
            value_in(global_map),
            value_in(repo_map),
            value_in(in_effect),
        ));
    }
}

/// The list header row, then one row per index up to the longer scope's
/// list. Lists concatenate across scopes (global first, then repo, trimmed
/// duplicates dropped), so the header carries the combined summary and the
/// member rows have no single effective value.
fn push_list_rows(
    rows: &mut Vec<ConfigFieldRow>,
    spec: &ConfigFieldSpec,
    child_prefix: &str,
    global: &serde_json::Value,
    repo: &serde_json::Value,
    dynamic: &Result<ResolvedDynamicWorkflows, String>,
) {
    let global_list = json_at(global, spec.name).and_then(|l| l.as_array());
    let repo_list = json_at(repo, spec.name).and_then(|l| l.as_array());
    let summary = |list: Option<&Vec<serde_json::Value>>| match list {
        Some(l) if !l.is_empty() => {
            let n = l.len();
            format!("{n} entr{}", if n == 1 { "y" } else { "ies" })
        }
        _ => "(none)".to_string(),
    };
    let effective = match dynamic {
        Err(error) => error.clone(),
        Ok(resolved) if resolved.guidance.is_empty() => "(none)".to_string(),
        Ok(resolved) => format!(
            "{} global + {} repo entries",
            resolved.guidance_counts.global, resolved.guidance_counts.repo
        ),
    };
    rows.push(header_row(
        spec,
        summary(global_list),
        summary(repo_list),
        effective,
    ));

    let len = |list: Option<&Vec<serde_json::Value>>| list.map_or(0, Vec::len);
    for i in 0..len(global_list).max(len(repo_list)) {
        let entry = |list: Option<&Vec<serde_json::Value>>| {
            list.and_then(|l| l.get(i))
                .and_then(|e| e.as_str())
                .map(str::to_string)
        };
        rows.push(member_row(
            format!("{child_prefix}{i}"),
            entry(global_list),
            entry(repo_list),
            None,
        ));
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigGetOutcome {
    /// The canonical field name, even when the input was a legacy alias.
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub effective_value: Option<String>,
    /// The legacy spelling the user typed, when they used one.
    pub alias_used: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSetOutcome {
    /// The canonical field name, even when the input was a legacy alias.
    pub field: String,
    pub value: String,
    pub scope: String,
    /// The write also moved `dynamicWorkflows` → `workflows.dynamic` (repo
    /// scope only).
    pub moved_legacy_dynamic_workflows: bool,
    /// The legacy spelling the user typed, when they used one.
    pub alias_used: Option<String>,
}

/// The `config migrate` confirmation's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateChoice {
    Move,
    Cancel,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigMigrateOutcome {
    pub scope: ConfigMigrationScope,
    /// The config file the migration reads and writes.
    pub path: String,
    /// Every step that has something to change; empty = nothing to migrate.
    pub changes: Vec<ConfigMigrationChange>,
    /// The path, then each change's summary and before/after JSON — the text
    /// the prompt body and `--dry-run` show. Empty when nothing changes.
    pub preview: String,
    /// The file was rewritten.
    pub applied: bool,
    /// `--dry-run`: nothing was written.
    pub dry_run: bool,
    /// `--global` only: the never-read `dynamicWorkflows` key is present.
    pub global_has_never_read_dynamic_workflows: bool,
    /// A line the command adds after the result (WI 0150 §3.12), so frontends
    /// print it without knowing any field name.
    pub note: Option<String>,
}

/// The `config migrate --global` note when the never-read key is present.
pub const GLOBAL_NEVER_READ_DYNAMIC_WORKFLOWS_NOTE: &str = "note: `dynamicWorkflows` in \
     ~/.awman/config.json was never read; move what you want to `workflows.dynamic`";

/// A user edit returned from the config show dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEditRequest {
    pub field: String,
    pub value: String,
    pub global: bool,
}

impl ConfigEditRequest {
    /// An edit to an existing row.
    pub fn to_field(field: impl Into<String>, value: impl Into<String>, global: bool) -> Self {
        Self {
            field: field.into(),
            value: value.into(),
            global,
        }
    }

    /// A new member, named `member`, of the map or list whose header has
    /// `shape`, written to the global scope when `global` (map and list
    /// members exist in both scopes).
    ///
    /// The field name is built from the header's own `child_prefix`, so a
    /// frontend adding an entry never spells a config path. `None` when the
    /// shape heads neither a map nor a list (WI 0114 F-20).
    pub fn new_member(
        shape: &ConfigRowShape,
        member: &str,
        value: impl Into<String>,
        global: bool,
    ) -> Option<Self> {
        let prefix = shape.child_prefix()?;
        Some(Self {
            field: format!("{prefix}{member}"),
            value: value.into(),
            global,
        })
    }
}

/// An edit that failed validation or could not be written to disk. Handed
/// back to the frontend on the next `present_config_table` call so the UI
/// can preserve the user's input and show the reason, instead of silently
/// discarding the edit.
#[derive(Debug, Clone)]
pub struct ConfigEditRejection {
    pub field: String,
    pub value: String,
    pub global: bool,
    pub reason: String,
}

pub trait ConfigCommandFrontend: UserMessageSink + Send + Sync {
    /// Present the config table to the user and block until they either
    /// dismiss the dialog or edit a value. Returns `Ok(None)` on dismiss,
    /// `Ok(Some(edit))` when the user changes a field.
    ///
    /// `rejected` carries the previous edit when it failed validation or
    /// could not be persisted; the frontend should surface the reason and
    /// let the user correct the value rather than retype it.
    fn present_config_table(
        &mut self,
        rows: &[ConfigFieldRow],
        rejected: Option<&ConfigEditRejection>,
    ) -> Result<Option<ConfigEditRequest>, CommandError>;

    /// Ask before `config migrate` rewrites a config file. The default
    /// refuses: there is no human to ask, so the caller must pass `--yes`
    /// (exit 2), as `AgentsCommandFrontend::confirm_kill` does. (The work item
    /// calls this the `MigrateConfigFrontend` prompt; it is a method here
    /// because the config command already owns one frontend trait.)
    fn confirm_config_migration(
        &mut self,
        _prompt: &Prompt<MigrateChoice>,
    ) -> Result<MigrateChoice, CommandError> {
        Err(CommandError::InteractiveInputUnavailable {
            prompt: "yes".into(),
        })
    }
}

pub struct ConfigCommand {
    sub: ConfigSubcommand,
    engines: Engines,
    session: crate::data::session::Session,
}

impl ConfigCommand {
    pub fn new(
        sub: ConfigSubcommand,
        engines: Engines,
        session: crate::data::session::Session,
    ) -> Self {
        Self {
            sub,
            engines,
            session,
        }
    }

    /// Construct from the catalogue-resolved input (WI 0113 F-10). The
    /// `config` leaves share one entry point, selected by the caller's
    /// canonical path.
    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let sub = match ctx.caller.leaf() {
            "show" => ConfigSubcommand::Show(ConfigShowFlags {}),
            "get" => ConfigSubcommand::Get(ConfigGetFlags {
                field: ctx.args.require("field")?,
            }),
            "set" => ConfigSubcommand::Set(ConfigSetFlags {
                field: ctx.args.require("field")?,
                value: ctx.args.require("value")?,
                global: ctx.flags.bool("global"),
            }),
            "migrate" => ConfigSubcommand::Migrate(ConfigMigrateFlags {
                yes: ctx.flags.bool("yes"),
                dry_run: ctx.flags.bool("dry-run"),
                global: ctx.flags.bool("global"),
                non_interactive: ctx.flags.bool("non-interactive"),
            }),
            "list" => ConfigSubcommand::PrerequisiteGrants(
                crate::command::commands::workflow_prerequisites::PrerequisiteGrantsAction::List,
            ),
            "revoke" => ConfigSubcommand::PrerequisiteGrants(
                crate::command::commands::workflow_prerequisites::PrerequisiteGrantsAction::Revoke(
                    ctx.args.require("selector")?,
                ),
            ),
            _ => return Err(CommandError::unknown_command(&ctx.path())),
        };
        Ok(Self::new(sub, ctx.engines.clone(), ctx.session.clone()))
    }

    pub fn subcommand(&self) -> &ConfigSubcommand {
        &self.sub
    }
}

/// Outcome enum used by the `Command` trait impl.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ConfigOutcome {
    Show(ConfigShowOutcome),
    Get(ConfigGetOutcome),
    Set(ConfigSetOutcome),
    Migrate(ConfigMigrateOutcome),
    PrerequisiteGrants(crate::command::commands::workflow_prerequisites::PrerequisiteGrantsOutcome),
}

fn info(text: &str) -> UserMessage {
    UserMessage {
        level: MessageLevel::Info,
        text: text.to_string(),
    }
}

#[async_trait]
impl Command for ConfigCommand {
    type Frontend = Box<dyn ConfigCommandFrontend>;
    type Outcome = ConfigOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let session = self.session;
        let notices = &self.engines.config_notices;
        let outcome = match self.sub {
            ConfigSubcommand::Show(_) => {
                notices.warn_legacy_dynamic_workflows(&session, frontend.as_mut());
                let mut session = session;
                let mut rejected: Option<ConfigEditRejection> = None;
                loop {
                    let (global, repo) = config_views(&session);
                    let rows = collect_config_rows(&global, &repo);

                    match frontend.present_config_table(&rows, rejected.take().as_ref())? {
                        None => {
                            break ConfigOutcome::Show(ConfigShowOutcome { global, repo, rows });
                        }
                        Some(edit) => {
                            // Any failure — validation, schema mismatch, or
                            // disk write — is handed back to the frontend as
                            // a rejection so the user's input is preserved
                            // and the reason is visible in the dialog. It is
                            // also logged for the post-dialog record.
                            let result = validate_and_coerce(&edit.field, &edit.value)
                                .and_then(|coerced| {
                                    if is_agent_field(&edit.field) {
                                        check_agent_known(
                                            &edit.value,
                                            &agent_listing_for_scope(&session, edit.global),
                                            edit.global,
                                        )?;
                                    }
                                    Ok(coerced)
                                })
                                .and_then(|coerced| {
                                    let field = resolve_config_field(&edit.field)
                                        .map(|resolved| resolved.canonical)
                                        .unwrap_or_else(|| edit.field.clone());
                                    write_config_field(&session, &field, coerced, edit.global)
                                });
                            match result {
                                Err(reason) => {
                                    frontend.write_message(UserMessage {
                                        level: MessageLevel::Warning,
                                        text: format!("Config edit not saved: {reason}"),
                                    });
                                    rejected = Some(ConfigEditRejection {
                                        field: edit.field,
                                        value: edit.value,
                                        global: edit.global,
                                        reason,
                                    });
                                    continue;
                                }
                                Ok(true) => frontend
                                    .write_message(info(MOVED_LEGACY_DYNAMIC_WORKFLOWS_NOTICE)),
                                Ok(false) => {}
                            }
                            session = {
                                let wd = session.working_dir().to_path_buf();
                                let gr = session.git_root().to_path_buf();
                                crate::data::session::Session::open_at_git_root(
                                    wd,
                                    gr,
                                    crate::data::session::SessionOpenOptions {
                                        env: Some(session.env().clone()),
                                        ..Default::default()
                                    },
                                )
                                .map_err(CommandError::from)?
                            };
                        }
                    }
                }
            }
            ConfigSubcommand::PrerequisiteGrants(action) => ConfigOutcome::PrerequisiteGrants(
                crate::command::commands::workflow_prerequisites::run_prerequisite_grants(
                    &session, &action,
                )?,
            ),
            ConfigSubcommand::Get(f) => {
                notices.warn_legacy_dynamic_workflows(&session, frontend.as_mut());
                let resolved = resolve_input_field(&f.field)?;
                let alias_used = resolved.via_alias.map(|_| f.field.clone());
                if alias_used.is_some() {
                    frontend.write_message(info(DYNAMIC_WORKFLOWS_ALIAS_NOTICE));
                }
                let (global, repo) = config_views(&session);
                let field = resolved.canonical;
                let (global_value, repo_value, effective_value) = match resolved.shape {
                    ConfigRowShape::Scalar => {
                        let global_value = config_field_value(&global, &json_path(&field));
                        let repo_value = config_field_value(&repo, &json_path(&field));
                        let effective_value = repo_value
                            .clone()
                            .or_else(|| global_value.clone())
                            .or_else(|| implicit_default(&field).map(str::to_string));
                        (global_value, repo_value, effective_value)
                    }
                    // Headers and members read exactly what `config show`
                    // displays for them, layering included.
                    _ => collect_config_rows(&global, &repo)
                        .into_iter()
                        .find(|row| row.field == field)
                        .map(|row| (row.global_value, row.repo_value, row.effective_value))
                        .unwrap_or_default(),
                };
                ConfigOutcome::Get(ConfigGetOutcome {
                    field,
                    global_value,
                    repo_value,
                    effective_value,
                    alias_used,
                })
            }
            ConfigSubcommand::Set(f) => {
                let resolved = resolve_input_field(&f.field)?;
                let field = resolved.canonical.clone();
                let usage = |reason: String| CommandError::InvalidFlagValue {
                    command: vec!["config".into(), "set".into()],
                    flag: "global".into(),
                    reason,
                };
                // A legacy name is only valid where the legacy key existed.
                if let Some(alias) = resolved.via_alias {
                    if alias.scope == FieldScope::RepoOnly && f.global {
                        return Err(usage(format!(
                            "'{}' is the old repo-only name; use '{field}' with --global",
                            f.field
                        )));
                    }
                }
                // Validate scope: enforce GlobalOnly / RepoOnly constraints.
                match resolved.spec.scope {
                    FieldScope::GlobalOnly if !f.global => {
                        return Err(usage(format!(
                            "field '{field}' can only be set in global config; add --global"
                        )));
                    }
                    FieldScope::RepoOnly if f.global => {
                        return Err(usage(format!(
                            "field '{field}' can only be set in repo config; omit --global"
                        )));
                    }
                    _ => {}
                }
                // Validate and coerce the value per field type.
                let invalid = |reason| CommandError::InvalidFlagValue {
                    command: vec!["config".into(), "set".into()],
                    flag: f.field.clone(),
                    reason,
                };
                let coerced = validate_and_coerce(&field, &f.value).map_err(invalid)?;
                if is_agent_field(&field) {
                    check_agent_known(
                        &f.value,
                        &agent_listing_for_scope(&session, f.global),
                        f.global,
                    )
                    .map_err(invalid)?;
                }
                let alias_used = resolved.via_alias.map(|_| f.field.clone());
                if alias_used.is_some() {
                    frontend.write_message(info(DYNAMIC_WORKFLOWS_ALIAS_NOTICE));
                }
                let moved =
                    write_config_field(&session, &field, coerced, f.global).map_err(|reason| {
                        CommandError::Other(format!("failed to set '{field}': {reason}"))
                    })?;
                if moved {
                    frontend.write_message(info(MOVED_LEGACY_DYNAMIC_WORKFLOWS_NOTICE));
                }
                ConfigOutcome::Set(ConfigSetOutcome {
                    field,
                    value: f.value,
                    scope: if f.global {
                        "global".into()
                    } else {
                        "repo".into()
                    },
                    moved_legacy_dynamic_workflows: moved,
                    alias_used,
                })
            }
            ConfigSubcommand::Migrate(f) => {
                ConfigOutcome::Migrate(run_migrate(&session, &f, frontend.as_mut())?)
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// A planned config migration of one scope, as Layer 0 builds it. Lets
/// `config migrate` drive either scope through one flow, so a future move
/// (a new `ConfigMigration` step in either scope) needs no new command code.
trait PlannedConfigMigration {
    fn path(&self) -> &std::path::Path;
    fn is_empty(&self) -> bool;
    fn changes(&self) -> &[ConfigMigrationChange];
    fn preview(&self) -> String;
    fn apply(self) -> Result<(), crate::data::error::DataError>;
}

impl PlannedConfigMigration for RepoConfigMigration {
    fn path(&self) -> &std::path::Path {
        RepoConfigMigration::path(self)
    }
    fn is_empty(&self) -> bool {
        RepoConfigMigration::is_empty(self)
    }
    fn changes(&self) -> &[ConfigMigrationChange] {
        RepoConfigMigration::changes(self)
    }
    fn preview(&self) -> String {
        RepoConfigMigration::preview(self)
    }
    fn apply(self) -> Result<(), crate::data::error::DataError> {
        RepoConfigMigration::apply(self)
    }
}

impl PlannedConfigMigration for GlobalConfigMigration {
    fn path(&self) -> &std::path::Path {
        GlobalConfigMigration::path(self)
    }
    fn is_empty(&self) -> bool {
        GlobalConfigMigration::is_empty(self)
    }
    fn changes(&self) -> &[ConfigMigrationChange] {
        GlobalConfigMigration::changes(self)
    }
    fn preview(&self) -> String {
        GlobalConfigMigration::preview(self)
    }
    fn apply(self) -> Result<(), crate::data::error::DataError> {
        GlobalConfigMigration::apply(self)
    }
}

/// `awman config migrate` (WI 0150 §4): plan every migration step for the
/// chosen scope, show it, ask, write.
fn run_migrate(
    session: &crate::data::session::Session,
    flags: &ConfigMigrateFlags,
    frontend: &mut dyn ConfigCommandFrontend,
) -> Result<ConfigMigrateOutcome, CommandError> {
    if flags.global {
        let path = crate::data::config::global::GlobalConfig::path_with(session.env())?;
        let plan = GlobalConfigMigration::plan(&path)?;
        let never_read = plan.has_never_read_dynamic_workflows();
        return run_migration_plan(
            plan,
            ConfigMigrationScope::Global,
            never_read,
            flags,
            frontend,
        );
    }
    // Only the invoking checkout's file, never a worktree copy. The plan
    // refuses a symlinked `.awman/` or `config.json` when it reads the file
    // and again when it writes it, consistent with `agent_dirs::publish`'s
    // treatment of repo-controlled symlinks.
    let plan = RepoConfigMigration::plan(session.git_root())?;
    run_migration_plan(plan, ConfigMigrationScope::Repo, false, flags, frontend)
}

fn run_migration_plan(
    plan: impl PlannedConfigMigration,
    scope: ConfigMigrationScope,
    global_has_never_read_dynamic_workflows: bool,
    flags: &ConfigMigrateFlags,
    frontend: &mut dyn ConfigCommandFrontend,
) -> Result<ConfigMigrateOutcome, CommandError> {
    let path = plan.path().to_path_buf();
    let mut outcome = ConfigMigrateOutcome {
        scope,
        path: path.display().to_string(),
        changes: plan.changes().to_vec(),
        preview: String::new(),
        applied: false,
        dry_run: flags.dry_run,
        global_has_never_read_dynamic_workflows,
        note: global_has_never_read_dynamic_workflows
            .then(|| GLOBAL_NEVER_READ_DYNAMIC_WORKFLOWS_NOTE.to_string()),
    };
    if plan.is_empty() {
        return Ok(outcome);
    }
    outcome.preview = plan.preview();
    if flags.dry_run {
        return Ok(outcome);
    }
    // Refuse before asking, so the user is never asked to approve a write
    // that cannot happen. An access check rather than the mode bits: a
    // writable-looking file owned by someone else is not writable either.
    if std::fs::metadata(&path).is_ok_and(|m| m.permissions().readonly())
        || (path.is_file() && std::fs::OpenOptions::new().write(true).open(&path).is_err())
    {
        return Err(CommandError::Other(format!(
            "{} is read-only; refusing to migrate it",
            path.display()
        )));
    }
    if !flags.yes {
        // Non-interactive mode never prompts: the decision must come from
        // `--yes` (exit 2), per aspec/uxui/cli.md.
        if flags.non_interactive {
            return Err(CommandError::InteractiveInputUnavailable {
                prompt: "yes".into(),
            });
        }
        let prompt = crate::command::prompts::config_migrate_confirm(&outcome.preview);
        if frontend.confirm_config_migration(&prompt)? == MigrateChoice::Cancel {
            return Err(CommandError::Aborted);
        }
    }
    plan.apply()?;
    outcome.applied = true;
    Ok(outcome)
}

/// Apply a coerced field value to a config snapshot, returning the updated
/// struct or a user-facing reason when the resulting JSON no longer fits the
/// config schema. Shared by the interactive edit loop and `config set` so a
/// schema mismatch is never silently dropped.
fn updated_config<T: Serialize + serde::de::DeserializeOwned>(
    cfg: &T,
    field: &str,
    coerced: serde_json::Value,
) -> Result<T, String> {
    let mut json =
        serde_json::to_value(cfg).map_err(|e| format!("could not serialize config: {e}"))?;
    apply_config_field(&mut json, &json_path(field), coerced);
    serde_json::from_value(json).map_err(|e| format!("'{field}' does not accept this value ({e})"))
}

/// Validate, apply, and persist one field edit to the chosen config scope.
/// Returns a user-facing reason on any failure — validation, schema
/// mismatch, or disk write — so callers can surface it instead of
/// pretending the edit succeeded.
///
/// A repo write is also the automatic `dynamicWorkflows` → `workflows.dynamic`
/// migration (WI 0150 §4): the legacy block is adopted before the edit is
/// applied, and `Ok(true)` reports that it moved so the caller can say so.
/// A repo config with both keys set is refused with the load error. A global
/// write never touches the repo file, and the reverse.
fn write_config_field(
    session: &crate::data::session::Session,
    field: &str,
    coerced: serde_json::Value,
    global: bool,
) -> Result<bool, String> {
    if global {
        let cfg = updated_config(session.global_config(), field, coerced)?;
        // The session's environment, never the process's: they differ in
        // tests and for any session opened with overrides.
        cfg.save_with(session.env())
            .map_err(|e| format!("could not write global config: {e}"))?;
        return Ok(false);
    }
    let mut cfg = session.repo_config().clone();
    cfg.dynamic_workflows_block().map_err(|e| e.to_string())?;
    let moved = cfg.adopt_legacy_dynamic_workflows();
    let cfg = updated_config(&cfg, field, coerced)?;
    cfg.save(session.git_root()).map_err(|e| {
        format!(
            "could not write {}: {e}",
            crate::data::config::repo::RepoConfig::path(session.git_root()).display()
        )
    })?;
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::config::config_json::{remove_config_field, set_config_field};

    #[test]
    fn api_work_dir_roots_parses_as_comma_list_like_work_dirs() {
        for field in ["api.workDirs", "api.workDirRoots"] {
            assert_eq!(
                validate_and_coerce(field, " /a, /b ,,/c ").unwrap(),
                serde_json::json!(["/a", "/b", "/c"]),
                "{field}"
            );
        }
        assert!(valid_field_names().contains(&"api.workDirRoots"));
    }

    // ── config_field_value ───────────────────────────────────────────────────

    #[test]
    fn config_field_value_returns_string_field() {
        let json = serde_json::json!({"agent": "claude", "model": null});
        assert_eq!(
            config_field_value(&json, "agent"),
            Some("claude".to_string())
        );
    }

    #[test]
    fn config_field_value_returns_bool_field_as_string() {
        let json = serde_json::json!({"yolo": true});
        assert_eq!(config_field_value(&json, "yolo"), Some("true".to_string()));
    }

    #[test]
    fn config_field_value_returns_number_field_as_string() {
        let json = serde_json::json!({"port": 9876u64});
        assert_eq!(config_field_value(&json, "port"), Some("9876".to_string()));
    }

    #[test]
    fn config_field_value_returns_none_for_missing_field() {
        let json = serde_json::json!({"agent": "claude"});
        assert_eq!(config_field_value(&json, "nonexistent"), None);
    }

    #[test]
    fn config_field_value_returns_none_for_null_value() {
        let json = serde_json::json!({"model": null});
        assert_eq!(config_field_value(&json, "model"), None);
    }

    #[test]
    fn config_field_value_supports_dot_notation() {
        let json = serde_json::json!({"work_items": {"dir": "aspec/work-items"}});
        assert_eq!(
            config_field_value(&json, "work_items.dir"),
            Some("aspec/work-items".to_string())
        );
    }

    // ── set_config_field ─────────────────────────────────────────────────────

    #[test]
    fn set_config_field_inserts_string_value() {
        let mut json = serde_json::json!({});
        set_config_field(
            &mut json,
            "agent",
            serde_json::Value::String("codex".into()),
        );
        assert_eq!(json["agent"], serde_json::Value::String("codex".into()));
    }

    #[test]
    fn set_config_field_inserts_bool_value() {
        let mut json = serde_json::json!({});
        set_config_field(&mut json, "yolo", serde_json::Value::Bool(true));
        assert_eq!(json["yolo"], serde_json::Value::Bool(true));
    }

    #[test]
    fn set_config_field_inserts_number_value() {
        let mut json = serde_json::json!({});
        set_config_field(&mut json, "port", serde_json::json!(9876u64));
        assert_eq!(json["port"], serde_json::json!(9876u64));
    }

    #[test]
    fn set_config_field_overwrites_existing_value() {
        let mut json = serde_json::json!({"agent": "claude"});
        set_config_field(
            &mut json,
            "agent",
            serde_json::Value::String("gemini".into()),
        );
        assert_eq!(json["agent"], serde_json::Value::String("gemini".into()));
    }

    #[test]
    fn set_config_field_does_not_modify_non_object() {
        // If the json is not an Object, set_config_field is a no-op.
        let mut json = serde_json::Value::Null;
        set_config_field(
            &mut json,
            "agent",
            serde_json::Value::String("claude".into()),
        );
        // Should still be Null — no panic.
        assert!(json.is_null());
    }

    #[test]
    fn set_config_field_dot_notation_creates_nested() {
        let mut json = serde_json::json!({});
        set_config_field(
            &mut json,
            "work_items.dir",
            serde_json::Value::String("custom/dir".into()),
        );
        assert_eq!(json["work_items"]["dir"], "custom/dir");
    }

    #[test]
    fn set_config_field_dot_notation_preserves_siblings() {
        let mut json = serde_json::json!({"work_items": {"template": "tmpl.md"}});
        set_config_field(
            &mut json,
            "work_items.dir",
            serde_json::Value::String("custom/dir".into()),
        );
        assert_eq!(json["work_items"]["dir"], "custom/dir");
        assert_eq!(json["work_items"]["template"], "tmpl.md");
    }

    // ── set_config_field: dynamicWorkflows.guidance array append (WI-0099) ────

    #[test]
    fn set_config_field_guidance_appends_to_new_array() {
        let mut json = serde_json::json!({});
        set_config_field(
            &mut json,
            "dynamicWorkflows.guidance.0",
            serde_json::Value::String("first entry".into()),
        );
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["first entry"]),
            "setting index 0 on an absent array must create it: {json}"
        );
    }

    #[test]
    fn set_config_field_guidance_appends_at_current_length() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {"guidance": ["first entry"]}
        });
        set_config_field(
            &mut json,
            "dynamicWorkflows.guidance.1",
            serde_json::Value::String("second entry".into()),
        );
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["first entry", "second entry"]),
            "index == current length must append, not overwrite: {json}"
        );
    }

    #[test]
    fn set_config_field_guidance_overwrites_in_place_when_index_in_range() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {"guidance": ["a", "b"]}
        });
        set_config_field(
            &mut json,
            "dynamicWorkflows.guidance.0",
            serde_json::Value::String("a-edited".into()),
        );
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["a-edited", "b"]),
            "index < length must overwrite in place: {json}"
        );
    }

    // ── remove_config_field: dynamicWorkflows.guidance by index (WI-0099) ─────

    #[test]
    fn remove_config_field_guidance_by_index_compacts_and_reindexes() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {"guidance": ["a", "b", "c"]}
        });
        remove_config_field(&mut json, "dynamicWorkflows.guidance.1");
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["a", "c"]),
            "removing index 1 must leave a compact, re-indexed array: {json}"
        );
    }

    #[test]
    fn remove_config_field_guidance_out_of_range_index_is_noop() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {"guidance": ["a"]}
        });
        remove_config_field(&mut json, "dynamicWorkflows.guidance.5");
        assert_eq!(
            json["dynamicWorkflows"]["guidance"],
            serde_json::json!(["a"]),
            "an out-of-range index must not modify the array: {json}"
        );
    }

    // ── validate_and_coerce ──────────────────────────────────────────────────

    #[test]
    fn validate_and_coerce_agent_valid() {
        let v = validate_and_coerce("agent", "claude").unwrap();
        assert_eq!(v, serde_json::Value::String("claude".into()));
    }

    #[test]
    fn validate_and_coerce_runtime_accepts_only_known_runtimes() {
        assert_eq!(
            validate_and_coerce("runtime", "builtin-experimental").unwrap(),
            serde_json::Value::String("builtin-experimental".into())
        );
        let err = validate_and_coerce("runtime", "builtin").unwrap_err();
        assert!(err.contains("builtin-experimental"), "{err}");
        assert!(validate_and_coerce("runtime", "dokcer").is_err());
    }

    // ── validate_and_coerce: builtin.builder.enabled (WI 0125) ──────────────

    #[test]
    fn validate_and_coerce_builder_enabled_accepts_true_and_false() {
        assert_eq!(
            validate_and_coerce("builtin.builder.enabled", "true").unwrap(),
            serde_json::Value::Bool(true)
        );
        assert_eq!(
            validate_and_coerce("builtin.builder.enabled", "false").unwrap(),
            serde_json::Value::Bool(false)
        );
        // `parse_enabled` trims surrounding whitespace.
        assert_eq!(
            validate_and_coerce("builtin.builder.enabled", "  true  ").unwrap(),
            serde_json::Value::Bool(true)
        );
    }

    /// `validate_and_coerce` routes `builtin.builder.enabled` through
    /// `BuiltinBuilderConfig::parse_enabled` (not the generic bool/string
    /// fallback), so the error names the field and rejects truthy spellings
    /// a generic bool coercion might otherwise accept.
    #[test]
    fn validate_and_coerce_builder_enabled_rejects_non_boolean_spellings() {
        for bad in ["yes", "1", "TRUE", ""] {
            let err = validate_and_coerce("builtin.builder.enabled", bad).unwrap_err();
            assert!(
                err.contains("builtin.builder.enabled"),
                "error for {bad:?} must name the field: {err}"
            );
        }
    }

    /// `config show`/`get` must show the documented default (`false`) as the
    /// *effective* value when nothing sets it, while leaving the global and
    /// repo cells unset so scope inheritance stays visible (review-config
    /// finding 3).
    #[test]
    fn collect_config_rows_builder_enabled_effective_default_is_false_but_cells_stay_unset() {
        let rows = collect_config_rows(&serde_json::json!({}), &serde_json::json!({}));
        let row = rows
            .iter()
            .find(|r| r.field == "builtin.builder.enabled")
            .expect("builtin.builder.enabled row must be present");
        assert_eq!(row.effective_value.as_deref(), Some("false"));
        assert_eq!(row.global_value, None);
        assert_eq!(row.repo_value, None);
    }

    /// An explicit repo `false` still displays as the effective value even
    /// when a global `true` is set — repo overrides global, not merely
    /// "inherits unless absent".
    #[test]
    fn collect_config_rows_builder_enabled_repo_false_overrides_global_true() {
        let global = serde_json::json!({"builtin": {"builder": {"enabled": true}}});
        let repo = serde_json::json!({"builtin": {"builder": {"enabled": false}}});
        let rows = collect_config_rows(&global, &repo);
        let row = rows
            .iter()
            .find(|r| r.field == "builtin.builder.enabled")
            .unwrap();
        assert_eq!(row.global_value.as_deref(), Some("true"));
        assert_eq!(row.repo_value.as_deref(), Some("false"));
        assert_eq!(row.effective_value.as_deref(), Some("false"));
    }

    #[test]
    fn validate_and_coerce_agent_invalid() {
        let listing = crate::engine::agent::registry::AgentListing {
            first_party: crate::engine::agent::first_party::FIRST_PARTY_AGENTS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ..Default::default()
        };
        assert!(validate_and_coerce("agent", "notareal").is_ok());
        let err = check_agent_known("notareal", &listing, false).unwrap_err();
        assert!(err.contains("not a known agent"));
        assert!(check_agent_known("claude", &listing, true).is_ok());
        assert!(validate_and_coerce("agent", "bad name!").is_err());
    }

    #[test]
    fn validate_and_coerce_list_field() {
        let v = validate_and_coerce("yoloDisallowedTools", "tool1, tool2, tool3").unwrap();
        assert_eq!(v, serde_json::json!(["tool1", "tool2", "tool3"]));
    }

    #[test]
    fn validate_and_coerce_number_field() {
        let v = validate_and_coerce("terminal_scrollback_lines", "5000").unwrap();
        assert_eq!(v, serde_json::json!(5000u64));
    }

    #[test]
    fn validate_and_coerce_number_field_invalid() {
        let err = validate_and_coerce("terminal_scrollback_lines", "abc").unwrap_err();
        assert!(err.contains("not a valid number"));
    }

    #[test]
    fn validate_and_coerce_default_bool() {
        assert_eq!(
            validate_and_coerce("some_field", "true").unwrap(),
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn validate_and_coerce_default_string() {
        assert_eq!(
            validate_and_coerce("some_field", "hello").unwrap(),
            serde_json::Value::String("hello".into())
        );
    }

    // ── validate_and_coerce: dynamicWorkflows (WI-0095) ─────────────────────

    #[test]
    fn validate_and_coerce_max_concurrent_steps_accepts_positive() {
        let v = validate_and_coerce("workflows.dynamic.maxConcurrentSteps", "3").unwrap();
        assert_eq!(v, serde_json::json!(3u64));
    }

    #[test]
    fn validate_and_coerce_max_concurrent_steps_rejects_zero() {
        let err = validate_and_coerce("workflows.dynamic.maxConcurrentSteps", "0").unwrap_err();
        assert!(
            err.contains("must be >= 1"),
            "zero must be rejected with the >= 1 message, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_default_leader_valid_shape() {
        let v = validate_and_coerce("workflows.dynamic.defaultLeader", "claude::claude-opus-4-8")
            .unwrap();
        assert_eq!(
            v,
            serde_json::Value::String("claude::claude-opus-4-8".into())
        );
    }

    #[test]
    fn validate_and_coerce_default_leader_invalid_shape() {
        let err = validate_and_coerce("workflows.dynamic.defaultLeader", "claude").unwrap_err();
        assert!(
            err.contains("expected agent::model"),
            "malformed leader must be rejected, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_rejects_agents_to_models_map() {
        // The map field itself cannot be written through `config set`;
        // without this rejection the coerced string would fail RepoConfig
        // deserialization and the set would silently write nothing.
        let err =
            validate_and_coerce("workflows.dynamic.agentsToModels", "claude:foo").unwrap_err();
        assert!(
            err.contains("workflows.dynamic.agentsToModels.claude"),
            "the map rejection must point at the per-agent syntax, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_agents_to_models_entry_parses_comma_list() {
        let v = validate_and_coerce(
            "workflows.dynamic.agentsToModels.claude",
            "opus-4-8, sonnet-4-6",
        )
        .unwrap();
        assert_eq!(v, serde_json::json!(["opus-4-8", "sonnet-4-6"]));
    }

    #[test]
    fn validate_and_coerce_agents_to_models_entry_empty_value_means_remove() {
        let v = validate_and_coerce("workflows.dynamic.agentsToModels.claude", "  ").unwrap();
        assert!(
            v.is_null(),
            "an empty model list must coerce to Null (entry removal), got: {v:?}"
        );
    }

    /// The map key *is* an agent name, so the rule and the wording are
    /// `AgentName`'s (F-20). Asserting the same message `AgentName::new`
    /// produces is what keeps this from becoming a fourth copy of the rule.
    #[test]
    fn validate_and_coerce_agents_to_models_entry_rejects_bad_agent_key() {
        let err =
            validate_and_coerce("workflows.dynamic.agentsToModels.bad name!", "m1").unwrap_err();
        let from_layer_0 = crate::data::session::AgentName::new("bad name!")
            .unwrap_err()
            .to_string();
        assert_eq!(
            err, from_layer_0,
            "the rejection must be Layer 0's, verbatim"
        );
    }

    /// The whole round trip a TUI user takes: a key the dialog accepts is one
    /// `config set` accepts, and a key it rejects is one `config set` rejects.
    #[test]
    fn the_dialogs_key_rule_and_the_writers_key_rule_are_the_same_rule() {
        for key in ["claude", "gpt-4", "my_agent", "a", &"x".repeat(64)] {
            assert!(
                crate::data::session::AgentName::new(key).is_ok(),
                "{key} must be accepted"
            );
            assert!(
                validate_and_coerce(&format!("workflows.dynamic.agentsToModels.{key}"), "m1")
                    .is_ok(),
                "{key} must be writable"
            );
        }
        // An empty key never forms a field name at all — `is_valid_field_name`
        // rejects `workflows.dynamic.agentsToModels.` before validation — so
        // only non-empty rejections are comparable here.
        for key in ["bad name!", "a/b", &"x".repeat(65)] {
            assert!(
                crate::data::session::AgentName::new(key).is_err(),
                "{key:?} must be rejected"
            );
            assert!(
                validate_and_coerce(&format!("workflows.dynamic.agentsToModels.{key}"), "m1")
                    .is_err(),
                "{key:?} must not be writable"
            );
        }
        assert!(crate::data::session::AgentName::new("").is_err());
        assert!(resolve_config_field("workflows.dynamic.agentsToModels.").is_none());
    }

    // ── validate_and_coerce: workflows.dynamic.guidance (WI-0099) ────────────

    #[test]
    fn validate_and_coerce_rejects_bare_guidance_field() {
        // The bare array field can't be set as a single string; the coerced
        // string would fail RepoConfig deserialization and silently write
        // nothing, so it must be rejected up front pointing at the per-index
        // syntax.
        let err =
            validate_and_coerce("workflows.dynamic.guidance", "some instruction").unwrap_err();
        assert!(
            err.contains("workflows.dynamic.guidance.0"),
            "the list rejection must point at the per-index syntax, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_guidance_entry_valid() {
        let v = validate_and_coerce(
            "workflows.dynamic.guidance.0",
            "never spawn more than two agents in parallel",
        )
        .unwrap();
        assert_eq!(
            v,
            serde_json::Value::String("never spawn more than two agents in parallel".into())
        );
    }

    #[test]
    fn validate_and_coerce_guidance_entry_is_not_comma_split() {
        // Unlike agentsToModels, a guidance entry is one free-form string that
        // may itself contain commas.
        let v =
            validate_and_coerce("workflows.dynamic.guidance.0", "do X, then Y, then Z").unwrap();
        assert_eq!(v, serde_json::Value::String("do X, then Y, then Z".into()));
    }

    #[test]
    fn validate_and_coerce_guidance_entry_empty_value_means_remove() {
        let v = validate_and_coerce("workflows.dynamic.guidance.0", "   ").unwrap();
        assert!(
            v.is_null(),
            "an empty/whitespace-only guidance value must coerce to Null (entry removal), got: {v:?}"
        );
    }

    #[test]
    fn validate_and_coerce_guidance_entry_rejects_over_length_cap() {
        let too_long = "a".repeat(crate::data::config::repo::GUIDANCE_MAX_ENTRY_LEN + 1);
        let err = validate_and_coerce("workflows.dynamic.guidance.0", &too_long).unwrap_err();
        assert!(
            err.contains("maximum"),
            "an over-length guidance entry must be rejected, got: {err}"
        );
    }

    // ── map/list member field name validation / scope ───────────────────────

    fn resolve_config_field_ok(name: &str) -> bool {
        resolve_config_field(name).is_some()
    }

    fn resolve_scope(name: &str) -> Option<FieldScope> {
        resolve_config_field(name).map(|resolved| resolved.spec.scope)
    }

    fn resolved_shape(name: &str) -> Option<ConfigRowShape> {
        resolve_config_field(name).map(|resolved| resolved.shape)
    }

    #[test]
    fn agents_to_models_entries_are_valid_fields_in_both_scopes() {
        assert!(resolve_config_field_ok(
            "workflows.dynamic.agentsToModels.claude"
        ));
        assert_eq!(
            resolve_scope("workflows.dynamic.agentsToModels.claude"),
            Some(FieldScope::Both)
        );
        // The bare map is a header, not a member; deeper paths are neither.
        assert_eq!(
            resolved_shape("workflows.dynamic.agentsToModels.claude"),
            Some(ConfigRowShape::MapEntry)
        );
        assert!(resolved_shape("workflows.dynamic.agentsToModels").is_some_and(|s| s.is_header()));
        assert!(!resolve_config_field_ok(
            "workflows.dynamic.agentsToModels.a.b"
        ));
    }

    #[test]
    fn guidance_entries_are_valid_fields_in_both_scopes() {
        assert!(resolve_config_field_ok("workflows.dynamic.guidance.0"));
        assert!(resolve_config_field_ok("workflows.dynamic.guidance.41"));
        assert_eq!(
            resolve_scope("workflows.dynamic.guidance.0"),
            Some(FieldScope::Both)
        );
        // The bare list is a header; non-numeric or dotted trailing segments
        // are not per-index guidance entries.
        assert!(resolved_shape("workflows.dynamic.guidance").is_some_and(|s| s.is_header()));
        assert!(!resolve_config_field_ok("workflows.dynamic.guidance.abc"));
        assert!(!resolve_config_field_ok("workflows.dynamic.guidance.0.1"));
    }

    // ── updated_config (silent-save-failure fix) ─────────────────────────────

    #[test]
    fn updated_config_applies_dynamic_workflows_fields() {
        let cfg = crate::data::config::repo::RepoConfig::default();
        let updated = updated_config(
            &cfg,
            "workflows.dynamic.defaultLeader",
            serde_json::Value::String("claude::claude-opus-4-8".into()),
        )
        .expect("a valid defaultLeader must round-trip through the schema");
        assert_eq!(
            updated
                .dynamic_workflows_block()
                .unwrap()
                .map(|(cfg, _)| cfg.clone())
                .expect("dynamicWorkflows block must be created")
                .default_leader
                .as_deref(),
            Some("claude::claude-opus-4-8")
        );
    }

    #[test]
    fn updated_config_reports_schema_mismatch_instead_of_dropping_edit() {
        // `workItems` is an object in the schema; a plain string used to be
        // silently discarded (nothing saved, no error shown). The mismatch
        // must now surface a reason naming the field.
        let cfg = crate::data::config::repo::RepoConfig::default();
        let err = updated_config(&cfg, "workItems", serde_json::Value::String("oops".into()))
            .expect_err("schema-mismatched values must be rejected, not ignored");
        assert!(
            err.contains("workItems"),
            "the reason must name the field: {err}"
        );
    }

    #[test]
    fn builtin_builder_cli_set_round_trips_as_nested_boolean() {
        let cfg = crate::data::config::repo::RepoConfig::default();
        let edited = updated_config(
            &cfg,
            "builtin.builder.enabled",
            validate_and_coerce("builtin.builder.enabled", "true").unwrap(),
        )
        .unwrap();
        let json = serde_json::to_value(&edited).unwrap();
        assert_eq!(json["builtin"]["builder"]["enabled"], true);
        assert!(json.get("builtin.builder.enabled").is_none());
        assert_eq!(
            validate_and_coerce("builtin.builder.enabled", "false").unwrap(),
            serde_json::json!(false)
        );
        assert!(validate_and_coerce("builtin.builder.enabled", "yes")
            .unwrap_err()
            .contains("builtin.builder.enabled"));
    }

    #[test]
    fn work_items_fields_are_written_and_read_under_the_work_items_key() {
        let cfg = crate::data::config::repo::RepoConfig::default();
        let edited = updated_config(&cfg, "work_items.dir", serde_json::json!("x")).unwrap();
        let edited =
            updated_config(&edited, "work_items.template", serde_json::json!("t.md")).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            edited.work_items_dir(tmp.path()),
            Some(tmp.path().join("x"))
        );
        let json = serde_json::to_value(&edited).unwrap();
        assert_eq!(json["workItems"]["template"], "t.md");
        assert!(json.get("work_items").is_none(), "{json}");

        let spec = field_spec("work_items.dir").unwrap();
        let row = scalar_row(spec, &serde_json::json!({}), &json);
        assert_eq!(row.repo_value.as_deref(), Some("x"));
    }

    // ── remove_config_field / apply_config_field ─────────────────────────────

    #[test]
    fn apply_config_field_null_removes_nested_entry() {
        let mut json = serde_json::json!({
            "dynamicWorkflows": {
                "agentsToModels": {"claude": ["a"], "codex": ["b"]}
            }
        });
        apply_config_field(
            &mut json,
            "dynamicWorkflows.agentsToModels.claude",
            serde_json::Value::Null,
        );
        assert!(
            json["dynamicWorkflows"]["agentsToModels"]
                .get("claude")
                .is_none(),
            "Null must remove the entry: {json}"
        );
        assert_eq!(
            json["dynamicWorkflows"]["agentsToModels"]["codex"],
            serde_json::json!(["b"]),
            "sibling entries must be preserved"
        );
    }

    #[test]
    fn remove_config_field_missing_path_is_noop() {
        let mut json = serde_json::json!({"agent": "claude"});
        remove_config_field(&mut json, "dynamicWorkflows.agentsToModels.claude");
        assert_eq!(json, serde_json::json!({"agent": "claude"}));
    }

    #[test]
    fn config_field_value_renders_string_arrays_comma_separated() {
        // List values display in the same shape the user types them, so
        // inline edits round-trip.
        let json = serde_json::json!({"yoloDisallowedTools": ["rm", "curl"]});
        assert_eq!(
            config_field_value(&json, "yoloDisallowedTools"),
            Some("rm, curl".to_string())
        );
    }

    // ── collect_config_rows: dynamicWorkflows (WI-0095) ─────────────────────

    #[test]
    fn collect_config_rows_includes_dynamic_workflows_scalar_fields() {
        let global = serde_json::json!({});
        let repo = serde_json::json!({
            "workflows": { "dynamic": {
                "defaultLeader": "claude::claude-opus-4-8",
                "maxConcurrentSteps": 3
            }}
        });
        let rows = collect_config_rows(&global, &repo);

        let leader_row = rows
            .iter()
            .find(|r| r.field == "workflows.dynamic.defaultLeader")
            .expect("flattened workflows.dynamic.defaultLeader row must be present");
        assert_eq!(
            leader_row.repo_value.as_deref(),
            Some("claude::claude-opus-4-8")
        );
        assert!(!leader_row.read_only, "defaultLeader stays inline-editable");

        let steps_row = rows
            .iter()
            .find(|r| r.field == "workflows.dynamic.maxConcurrentSteps")
            .expect("flattened workflows.dynamic.maxConcurrentSteps row must be present");
        assert_eq!(steps_row.repo_value.as_deref(), Some("3"));
        assert!(
            !steps_row.read_only,
            "maxConcurrentSteps stays inline-editable"
        );
    }

    #[test]
    fn collect_config_rows_expands_agents_to_models_into_per_agent_rows() {
        let global = serde_json::json!({});
        // The repo view `config show` reads (a legacy block is adopted into
        // `workflows.dynamic` before rows are collected).
        let repo = serde_json::json!({
            "workflows": {"dynamic": {
                "agentsToModels": {
                    "claude": ["claude-opus-4-8", "claude-sonnet-4-6"],
                    "codex": ["codex-mini-latest"]
                }
            }}
        });
        let rows = collect_config_rows(&global, &repo);

        // The map itself appears as a read-only summary row (never a raw
        // JSON blob), so the mapping is discoverable even when empty.
        let summary_row = rows
            .iter()
            .find(|r| r.field == "workflows.dynamic.agentsToModels")
            .expect("agentsToModels summary row must be present");
        assert_eq!(summary_row.repo_value.as_deref(), Some("2 agents mapped"));
        assert!(summary_row.read_only, "the summary row is not editable");

        let claude_row = rows
            .iter()
            .find(|r| r.field == "workflows.dynamic.agentsToModels.claude")
            .expect("per-agent row for claude must be present");
        assert_eq!(
            claude_row.repo_value.as_deref(),
            Some("claude-opus-4-8, claude-sonnet-4-6")
        );
        assert!(
            !claude_row.read_only,
            "agentsToModels per-agent rows must be inline-editable"
        );
        assert!(
            claude_row.global_writable && claude_row.repo_writable,
            "per-agent rows are writable in both scopes"
        );

        let codex_row = rows
            .iter()
            .find(|r| r.field == "workflows.dynamic.agentsToModels.codex")
            .expect("per-agent row for codex must be present");
        assert_eq!(codex_row.repo_value.as_deref(), Some("codex-mini-latest"));
    }

    #[test]
    fn collect_config_rows_shows_empty_map_summary_when_unset() {
        let rows = collect_config_rows(&serde_json::json!({}), &serde_json::json!({}));
        let summary_row = rows
            .iter()
            .find(|r| r.field == "workflows.dynamic.agentsToModels")
            .expect("summary row must be present even with no mapping configured");
        assert_eq!(summary_row.repo_value.as_deref(), Some("(none)"));
    }

    // ── removed_field_message ────────────────────────────────────────────────

    #[test]
    fn env_passthrough_is_recognized_as_removed_field() {
        let msg = removed_field_message("envPassthrough")
            .expect("envPassthrough must be flagged as a removed field");
        assert!(
            msg.contains("env(VAR_NAME)") || msg.contains("overlays"),
            "removed-field message must steer users to the env() overlay form; got: {msg}"
        );
    }

    #[test]
    fn unknown_field_returns_no_removed_message() {
        assert!(removed_field_message("agent").is_none());
        assert!(removed_field_message("notARealField").is_none());
    }

    #[test]
    fn env_passthrough_is_not_in_valid_fields() {
        let names = valid_field_names();
        assert!(
            !names.contains(&"envPassthrough"),
            "envPassthrough must be removed from VALID_CONFIG_FIELDS"
        );
    }

    // ── field_scope ──────────────────────────────────────────────────────────

    #[test]
    fn field_scope_global_only() {
        assert_eq!(resolve_scope("runtime"), Some(FieldScope::GlobalOnly));
    }

    #[test]
    fn field_scope_repo_only() {
        assert_eq!(resolve_scope("work_items.dir"), Some(FieldScope::RepoOnly));
    }

    #[test]
    fn field_scope_both() {
        assert_eq!(resolve_scope("agent"), Some(FieldScope::Both));
    }

    // ── field_suggestions ─────────────────────────────────────────────────────

    #[test]
    fn field_suggestions_finds_close_match() {
        let names = valid_field_names();
        let result = field_suggestions("agnet", &names);
        // "agnet" is distance 2 from "agent" (two transpositions); should appear.
        assert!(
            result.contains(&"agent"),
            "suggestions must contain 'agent' for input 'agnet': {result:?}"
        );
    }

    #[test]
    fn field_suggestions_empty_when_no_close_match() {
        let names = valid_field_names();
        let result = field_suggestions("zzzzzzzzzzz", &names);
        assert!(
            result.is_empty(),
            "suggestions must be empty for very distant input"
        );
    }

    #[test]
    fn field_suggestions_sorted_by_distance() {
        let names = valid_field_names();
        // "runtim" is distance 1 from "runtime" and distance 2+ from all others.
        let result = field_suggestions("runtim", &names);
        if result.len() >= 2 {
            // First result must be "runtime" (closest match).
            assert_eq!(
                result[0], "runtime",
                "closest match must be first: {result:?}"
            );
        }
    }
}

#[cfg(test)]
mod edit_loop_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Scripted frontend for the interactive `config show` edit loop: returns
    /// the next canned response on each `present_config_table` call and
    /// records the rejection (if any) it was shown alongside the table.
    struct ScriptedConfigFrontend {
        responses: Vec<Option<ConfigEditRequest>>,
        seen_rejections: Arc<Mutex<Vec<Option<ConfigEditRejection>>>>,
    }

    impl crate::data::message::UserMessageSink for ScriptedConfigFrontend {
        fn write_message(&mut self, _msg: crate::data::message::UserMessage) {}
        fn replay_queued(&mut self) {}
    }

    impl ConfigCommandFrontend for ScriptedConfigFrontend {
        fn present_config_table(
            &mut self,
            _rows: &[ConfigFieldRow],
            rejected: Option<&ConfigEditRejection>,
        ) -> Result<Option<ConfigEditRequest>, CommandError> {
            self.seen_rejections.lock().unwrap().push(rejected.cloned());
            Ok(self.responses.remove(0))
        }
    }

    fn open_session(git_root: &std::path::Path) -> crate::data::session::Session {
        crate::data::session::Session::for_tests(git_root)
    }

    #[tokio::test]
    async fn show_edit_loop_preserves_rejected_input_and_persists_valid_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let frontend = ScriptedConfigFrontend {
            responses: vec![
                // 1: invalid leader — must come back as a rejection carrying
                // the typed value, not silently vanish.
                Some(ConfigEditRequest {
                    field: "workflows.dynamic.defaultLeader".into(),
                    value: "claude".into(),
                    global: false,
                }),
                // 2: corrected value — must persist to .awman/config.json.
                Some(ConfigEditRequest {
                    field: "workflows.dynamic.defaultLeader".into(),
                    value: "claude::claude-opus-4-8".into(),
                    global: false,
                }),
                // 3: dismiss the dialog.
                None,
            ],
            seen_rejections: seen.clone(),
        };

        let cmd = ConfigCommand::new(
            ConfigSubcommand::Show(ConfigShowFlags {}),
            crate::command::dispatch::Engines::for_tests(std::path::Path::new("/tmp")),
            open_session(tmp.path()),
        );
        cmd.run_with_frontend(Box::new(frontend))
            .await
            .expect("the edit loop must not error on a rejected value");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "the table must be presented three times");
        assert!(seen[0].is_none(), "no rejection on first present");
        let rej = seen[1]
            .as_ref()
            .expect("an invalid edit must be re-presented as a rejection");
        assert_eq!(rej.field, "workflows.dynamic.defaultLeader");
        assert_eq!(rej.value, "claude", "the user's input must be preserved");
        assert!(
            rej.reason.contains("expected agent::model"),
            "the rejection must carry the validation reason: {}",
            rej.reason
        );
        assert!(
            seen[2].is_none(),
            "a successful save must not carry a rejection"
        );

        let cfg = crate::data::config::repo::RepoConfig::load(tmp.path())
            .expect("the saved repo config must load cleanly");
        assert_eq!(
            cfg.dynamic_workflows_block()
                .unwrap()
                .map(|(cfg, _)| cfg.clone())
                .expect("dynamicWorkflows must be persisted")
                .default_leader
                .as_deref(),
            Some("claude::claude-opus-4-8"),
            "the corrected edit must be persisted to config.json"
        );
    }

    #[tokio::test]
    async fn set_reports_schema_mismatch_instead_of_silent_success() {
        let tmp = tempfile::tempdir().unwrap();
        let frontend = ScriptedConfigFrontend {
            responses: vec![],
            seen_rejections: Arc::new(Mutex::new(Vec::new())),
        };

        let cmd = ConfigCommand::new(
            ConfigSubcommand::Set(ConfigSetFlags {
                field: "workItems".into(),
                value: "not-an-object".into(),
                global: false,
            }),
            crate::command::dispatch::Engines::for_tests(std::path::Path::new("/tmp")),
            open_session(tmp.path()),
        );
        let err = cmd
            .run_with_frontend(Box::new(frontend))
            .await
            .expect_err("a value the schema cannot hold must fail the command");
        assert!(
            err.to_string().contains("workItems"),
            "the error must name the field: {err}"
        );
        assert!(
            !crate::data::config::repo::RepoConfig::path(tmp.path()).exists(),
            "nothing must be written when the set fails"
        );
    }
}

#[cfg(test)]
mod agent_scope_tests {
    use super::*;
    use crate::data::session::Session;

    const DEF: &str =
        "schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[image]\ninstall=['x']\n";

    fn fixture() -> (tempfile::TempDir, Session) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("global");
        std::fs::create_dir_all(home.join("agents")).unwrap();
        std::fs::create_dir_all(root.path().join(".awman/agents")).unwrap();
        std::fs::write(root.path().join(".awman/agents/repoonly.toml"), DEF).unwrap();
        std::fs::write(home.join("agents/globalonly.toml"), DEF).unwrap();
        let session = Session::for_tests_isolated(root.path(), &home);
        (root, session)
    }

    fn accepts(session: &Session, value: &str, global: bool) -> Result<(), String> {
        check_agent_known(value, &agent_listing_for_scope(session, global), global)
    }

    #[test]
    fn repo_scope_accepts_first_party_repo_and_global_custom() {
        let (_r, s) = fixture();
        for v in ["claude", "antigravity", "repoonly", "globalonly"] {
            assert!(accepts(&s, v, false).is_ok(), "{v}");
        }
    }

    #[test]
    fn global_scope_accepts_first_party_and_global_custom_only() {
        let (_r, s) = fixture();
        for v in ["codex", "globalonly"] {
            assert!(accepts(&s, v, true).is_ok(), "{v}");
        }
        let err = accepts(&s, "repoonly", true).unwrap_err();
        assert!(err.contains("not a known agent for global config"), "{err}");
        assert!(err.ends_with("awman new agent repoonly --global"), "{err}");
    }

    #[test]
    fn unknown_name_lists_valid_agents_and_the_new_agent_hint() {
        let (_r, s) = fixture();
        let err = accepts(&s, "nope", false).unwrap_err();
        assert!(err.contains("not a known agent for repo config"));
        assert!(err.contains("claude") && err.contains("repoonly") && err.contains("globalonly"));
        assert!(err.ends_with("awman new agent nope"), "{err}");
    }

    #[test]
    fn default_agent_field_is_checked_like_agent() {
        assert!(is_agent_field("agent") && is_agent_field("default_agent"));
        assert!(!is_agent_field("model"));
        // Shape check stays in validate_and_coerce.
        assert!(validate_and_coerce("default_agent", "pi").is_ok());
        assert!(validate_and_coerce("default_agent", "Bad Name").is_err());
    }

    #[test]
    fn removed_agent_values_constant_is_gone() {
        let src = include_str!("config.rs");
        assert!(!src.contains(concat!("VALID_AGENT", "_VALUES")));
    }
}

#[cfg(test)]
mod wi0150_command_tests {
    use super::*;
    use crate::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME};
    use crate::data::message::UserMessage;
    use crate::data::session::Session;

    struct MigrationFrontend(MigrateChoice);
    impl crate::data::message::UserMessageSink for MigrationFrontend {
        fn write_message(&mut self, _msg: UserMessage) {}
        fn replay_queued(&mut self) {}
    }
    impl ConfigCommandFrontend for MigrationFrontend {
        fn present_config_table(
            &mut self,
            _rows: &[ConfigFieldRow],
            _rejected: Option<&ConfigEditRejection>,
        ) -> Result<Option<ConfigEditRequest>, CommandError> {
            Ok(None)
        }
        fn confirm_config_migration(
            &mut self,
            _prompt: &Prompt<MigrateChoice>,
        ) -> Result<MigrateChoice, CommandError> {
            Ok(self.0)
        }
    }

    #[derive(Clone, Default)]
    struct RecordingFrontend {
        messages: std::sync::Arc<std::sync::Mutex<Vec<UserMessage>>>,
    }
    impl crate::data::message::UserMessageSink for RecordingFrontend {
        fn write_message(&mut self, msg: UserMessage) {
            self.messages.lock().unwrap().push(msg);
        }
        fn replay_queued(&mut self) {}
    }
    impl ConfigCommandFrontend for RecordingFrontend {
        fn present_config_table(
            &mut self,
            _rows: &[ConfigFieldRow],
            _rejected: Option<&ConfigEditRejection>,
        ) -> Result<Option<ConfigEditRequest>, CommandError> {
            Ok(None)
        }
    }

    fn session(root: &std::path::Path, home: &std::path::Path) -> Session {
        Session::for_tests_with_env(
            root,
            EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, home.to_str().unwrap())]),
        )
    }
    fn legacy_repo(root: &std::path::Path) -> std::path::PathBuf {
        let path = crate::data::config::repo::RepoConfig::path(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\n  \"agent\": \"claude\",\n  \"dynamicWorkflows\": {\"guidance\":[\"old\"]}\n}\n",
        )
        .unwrap();
        path
    }
    fn flags(yes: bool, dry_run: bool, global: bool, non_interactive: bool) -> ConfigMigrateFlags {
        ConfigMigrateFlags {
            yes,
            dry_run,
            global,
            non_interactive,
        }
    }

    #[test]
    fn config_migrate_prompt_accept_applies_migration() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let path = legacy_repo(root.path());
        let session = session(root.path(), home.path());
        let result = run_migrate(
            &session,
            &flags(false, false, false, false),
            &mut MigrationFrontend(MigrateChoice::Move),
        )
        .unwrap();
        assert!(result.applied);
        assert!(std::fs::read_to_string(path).unwrap().contains("workflows"));
    }

    #[test]
    fn config_migrate_cancel_leaves_file_byte_for_byte_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let path = legacy_repo(root.path());
        let before = std::fs::read(&path).unwrap();
        let session = session(root.path(), home.path());
        let err = run_migrate(
            &session,
            &flags(false, false, false, false),
            &mut MigrationFrontend(MigrateChoice::Cancel),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 130);
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn config_migrate_yes_applies_without_prompt() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        legacy_repo(root.path());
        let session = session(root.path(), home.path());
        assert!(
            run_migrate(
                &session,
                &flags(true, false, false, true),
                &mut MigrationFrontend(MigrateChoice::Cancel)
            )
            .unwrap()
            .applied
        );
    }

    #[test]
    fn config_migrate_dry_run_previews_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let path = legacy_repo(root.path());
        let before = std::fs::read(&path).unwrap();
        let session = session(root.path(), home.path());
        let result = run_migrate(
            &session,
            &flags(false, true, false, false),
            &mut MigrationFrontend(MigrateChoice::Move),
        )
        .unwrap();
        assert!(result.dry_run && !result.applied && !result.preview.is_empty());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn config_migrate_non_interactive_without_yes_exits_two() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        legacy_repo(root.path());
        let session = session(root.path(), home.path());
        let err = run_migrate(
            &session,
            &flags(false, false, false, true),
            &mut MigrationFrontend(MigrateChoice::Move),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn config_migrate_nothing_to_migrate_exits_zero_and_both_keys_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".awman")).unwrap();
        std::fs::write(
            root.path().join(".awman/config.json"),
            "{\"agent\":\"claude\"}",
        )
        .unwrap();
        let session = session(root.path(), home.path());
        let result = run_migrate(
            &session,
            &flags(false, false, false, false),
            &mut MigrationFrontend(MigrateChoice::Move),
        )
        .unwrap();
        assert!(result.changes.is_empty() && !result.applied);
        let path = legacy_repo(root.path());
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let mut value = value;
        value["workflows"] = serde_json::json!({"dynamic": {}});
        std::fs::write(
            root.path().join(".awman/config.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        let err = run_migrate(
            &session,
            &flags(true, false, false, false),
            &mut MigrationFrontend(MigrateChoice::Move),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("dynamicWorkflows")
                && err.to_string().contains("workflows.dynamic")
        );
    }

    #[test]
    fn config_migrate_global_reports_nothing_to_migrate_without_rewriting() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = session(root.path(), home.path());
        let path = crate::data::config::global::GlobalConfig::path_with(session.env()).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"dynamicWorkflows\":{}}").unwrap();
        let before = std::fs::read(&path).unwrap();
        let result = run_migrate(
            &session,
            &flags(false, false, true, false),
            &mut MigrationFrontend(MigrateChoice::Move),
        )
        .unwrap();
        assert!(result.changes.is_empty() && result.global_has_never_read_dynamic_workflows);
        assert_eq!(
            result.note.as_deref(),
            Some(GLOBAL_NEVER_READ_DYNAMIC_WORKFLOWS_NOTE)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // Absent or null: no note.
        for raw in ["{}", "{\"dynamicWorkflows\":null}"] {
            std::fs::write(&path, raw).unwrap();
            let result = run_migrate(
                &session,
                &flags(true, false, true, false),
                &mut MigrationFrontend(MigrateChoice::Move),
            )
            .unwrap();
            assert!(result.note.is_none(), "{raw}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        }
    }

    /// `config set --global` writes the session's global file, never the one
    /// the process environment points at (they differ here: the session has
    /// an `AWMAN_CONFIG_HOME` override, the process does not).
    #[tokio::test]
    async fn config_set_global_writes_the_session_global_file_only() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = session(root.path(), home.path());
        let session_path =
            crate::data::config::global::GlobalConfig::path_with(session.env()).unwrap();
        let process_path = crate::data::config::global::GlobalConfig::path().unwrap();
        assert_ne!(session_path, process_path);
        let process_before = std::fs::read(&process_path).ok();
        ConfigCommand::new(
            ConfigSubcommand::Set(ConfigSetFlags {
                field: "workflows.dynamic.guidance.0".into(),
                value: "global note".into(),
                global: true,
            }),
            crate::command::dispatch::Engines::for_tests(root.path()),
            session,
        )
        .run_with_frontend(Box::new(MigrationFrontend(MigrateChoice::Move)))
        .await
        .unwrap();
        let global: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&session_path).unwrap()).unwrap();
        assert_eq!(global["workflows"]["dynamic"]["guidance"][0], "global note");
        assert_eq!(std::fs::read(&process_path).ok(), process_before);
        assert!(!crate::data::config::repo::RepoConfig::path(root.path()).exists());
    }

    /// An edit that would make the file fail to load is refused and the file
    /// is left as it was, so the command that made it can still undo it.
    #[tokio::test]
    async fn config_set_refuses_an_edit_that_would_make_the_scope_unloadable() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let global_path = crate::data::config::global::GlobalConfig::path_with(
            session(root.path(), home.path()).env(),
        )
        .unwrap();
        std::fs::create_dir_all(global_path.parent().unwrap()).unwrap();
        let fifty: Vec<String> = (0..50).map(|i| format!("entry {i}")).collect();
        let raw = serde_json::to_string(
            &serde_json::json!({"workflows": {"dynamic": {"guidance": fifty}}}),
        )
        .unwrap();
        std::fs::write(&global_path, &raw).unwrap();
        let repo_path = crate::data::config::repo::RepoConfig::path(root.path());
        std::fs::create_dir_all(repo_path.parent().unwrap()).unwrap();
        std::fs::write(&repo_path, &raw).unwrap();
        for global in [true, false] {
            let session = session(root.path(), home.path());
            let err = ConfigCommand::new(
                ConfigSubcommand::Set(ConfigSetFlags {
                    field: "workflows.dynamic.guidance.50".into(),
                    value: "extra".into(),
                    global,
                }),
                crate::command::dispatch::Engines::for_tests(root.path()),
                session,
            )
            .run_with_frontend(Box::new(MigrationFrontend(MigrateChoice::Move)))
            .await
            .unwrap_err();
            assert!(
                err.to_string().contains("workflows.dynamic.guidance"),
                "global={global}: {err}"
            );
        }
        assert_eq!(std::fs::read_to_string(&global_path).unwrap(), raw);
        assert_eq!(std::fs::read_to_string(&repo_path).unwrap(), raw);
    }

    #[tokio::test]
    async fn global_guidance_payload_persists_only_in_isolated_global_config_and_alias_is_usage_error(
    ) {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let session = session(root.path(), home.path());
        let updated = updated_config(
            session.global_config(),
            "workflows.dynamic.guidance.0",
            serde_json::json!("global note"),
        )
        .unwrap();
        updated.save_with(session.env()).unwrap();
        let global_path =
            crate::data::config::global::GlobalConfig::path_with(session.env()).unwrap();
        let global: serde_json::Value =
            serde_json::from_slice(&std::fs::read(global_path).unwrap()).unwrap();
        assert_eq!(global["workflows"]["dynamic"]["guidance"][0], "global note");
        assert!(!crate::data::config::repo::RepoConfig::path(root.path()).exists());
        let alias = resolve_input_field("dynamicWorkflows.guidance.0").unwrap();
        assert_eq!(
            alias.via_alias.unwrap().scope,
            crate::data::config::fields::FieldScope::RepoOnly
        );
        let err = ConfigCommand::new(
            ConfigSubcommand::Set(ConfigSetFlags {
                field: "dynamicWorkflows.guidance.0".into(),
                value: "x".into(),
                global: true,
            }),
            crate::command::dispatch::Engines::for_tests(root.path()),
            session,
        )
        .run_with_frontend(Box::new(MigrationFrontend(MigrateChoice::Move)))
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("old repo-only name"), "{err}");
    }

    #[tokio::test]
    async fn config_set_old_name_moves_repo_block_and_reports_move() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let path = legacy_repo(root.path());
        let session = session(root.path(), home.path());
        let frontend = RecordingFrontend::default();
        let messages = frontend.messages.clone();
        let result = ConfigCommand::new(
            ConfigSubcommand::Set(ConfigSetFlags {
                field: "dynamicWorkflows.guidance.1".into(),
                value: "new note".into(),
                global: false,
            }),
            crate::command::dispatch::Engines::for_tests(root.path()),
            session,
        )
        .run_with_frontend(Box::new(frontend))
        .await
        .unwrap();
        let ConfigOutcome::Set(outcome) = result else {
            panic!("expected set outcome")
        };
        assert_eq!(outcome.field, "workflows.dynamic.guidance.1");
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(saved.get("dynamicWorkflows").is_none());
        assert_eq!(saved["workflows"]["dynamic"]["guidance"][1], "new note");
        let messages = messages.lock().unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.text.contains("dynamicWorkflows.*")
                    && m.text.contains("workflows.dynamic.*")),
            "{messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.text.contains("also moved")),
            "{messages:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_migrate_refuses_symlink_and_read_only_repo_config() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("config.json");
        std::fs::write(&target, "{}").unwrap();
        std::fs::create_dir_all(root.path().join(".awman")).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join(".awman/config.json")).unwrap();
        let session = session(root.path(), home.path());
        let err = run_migrate(
            &session,
            &flags(true, false, false, false),
            &mut MigrationFrontend(MigrateChoice::Move),
        )
        .unwrap_err();
        assert!(err.to_string().contains("symlink"));
        std::fs::remove_file(root.path().join(".awman/config.json")).unwrap();
        let path = legacy_repo(root.path());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let err = run_migrate(
            &session,
            &flags(true, false, false, false),
            &mut MigrationFrontend(MigrateChoice::Move),
        )
        .unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    }
}
