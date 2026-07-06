//! `ConfigCommand` — view and edit global / repo configuration.

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::Engines;
use crate::command::error::CommandError;
use crate::data::message::UserMessageSink;

/// Scope metadata for each config field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldScope {
    /// May only be written to global config.
    GlobalOnly,
    /// May only be written to repo config.
    RepoOnly,
    /// May be written to either global or repo config.
    Both,
}

/// Entry in the config field table: `(dotted_name, scope)`.
const VALID_CONFIG_FIELDS: &[(&str, FieldScope)] = &[
    ("agent", FieldScope::Both),
    ("auto_agent_auth_accepted", FieldScope::GlobalOnly),
    ("terminal_scrollback_lines", FieldScope::Both),
    ("yoloDisallowedTools", FieldScope::Both),
    ("workItems", FieldScope::RepoOnly),
    ("overlays", FieldScope::Both),
    ("agentStuckTimeout", FieldScope::Both),
    ("runtime", FieldScope::GlobalOnly),
    ("default_agent", FieldScope::GlobalOnly),
    ("api", FieldScope::GlobalOnly),
    ("remote", FieldScope::Both),
    // Dot-notation nested fields
    ("work_items.dir", FieldScope::RepoOnly),
    ("work_items.template", FieldScope::RepoOnly),
    ("api.workDirs", FieldScope::GlobalOnly),
    ("api.port", FieldScope::GlobalOnly),
    ("api.background", FieldScope::GlobalOnly),
    ("remote.defaultAddr", FieldScope::Both),
    ("remote.defaultAPIKey", FieldScope::Both),
    // Dynamic-workflow config (WI-0095), repo-only.
    ("dynamicWorkflows.defaultLeader", FieldScope::RepoOnly),
    ("dynamicWorkflows.maxConcurrentSteps", FieldScope::RepoOnly),
    ("dynamicWorkflows.agentsToModels", FieldScope::RepoOnly),
];

/// Field names that were removed in WI-0082 (overlay unification). Naming any
/// of these in `awman config get|set` returns a guidance error instead of the
/// generic "unknown field" suggestion list.
const REMOVED_CONFIG_FIELDS: &[(&str, &str)] = &[(
    "envPassthrough",
    "the 'envPassthrough' field was removed; express env passthrough as overlay entries \
     instead (e.g. `awman config set overlays \"env(VAR_NAME)\"` or add `\"env(VAR_NAME)\"` \
     to the `overlays` array in `.awman/config.json`). See `docs/09-overlays.md`.",
)];

fn removed_field_message(name: &str) -> Option<&'static str> {
    REMOVED_CONFIG_FIELDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, msg)| *msg)
}

/// Flat list of all valid field names (for suggestions / validation).
fn valid_field_names() -> Vec<&'static str> {
    VALID_CONFIG_FIELDS.iter().map(|(name, _)| *name).collect()
}

/// Look up the scope for a field name.
fn field_scope(name: &str) -> Option<FieldScope> {
    VALID_CONFIG_FIELDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
}

/// Valid agent names for config set agent=<value>. Must stay in sync with
/// `engine::agent::agent_matrix::SUPPORTED_AGENTS` — the matrix is authoritative;
/// this list is checked at config-write time so unsupported values are rejected
/// before they reach the engine.
const VALID_AGENT_VALUES: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "opencode",
    "crush",
    "cline",
    "copilot",
    "maki",
    "antigravity",
];

/// Validate and coerce a string value into the appropriate JSON type for the
/// given field. Returns the coerced `serde_json::Value` or a user-facing error.
fn validate_and_coerce(field: &str, value: &str) -> Result<serde_json::Value, String> {
    match field {
        "agent" | "default_agent" => {
            if !VALID_AGENT_VALUES.contains(&value) {
                return Err(format!(
                    "'{}' is not a known agent; valid agents: {}",
                    value,
                    VALID_AGENT_VALUES.join(", ")
                ));
            }
            Ok(serde_json::Value::String(value.to_string()))
        }
        "yoloDisallowedTools" | "overlays" | "api.workDirs" => {
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
        "dynamicWorkflows.maxConcurrentSteps" => {
            // Positive integer; zero would deadlock any workflow (WI-0095).
            let n = value
                .parse::<u64>()
                .map_err(|_| format!("'{}' is not a valid number", value))?;
            if n < 1 {
                return Err("dynamicWorkflows.maxConcurrentSteps must be >= 1".to_string());
            }
            Ok(serde_json::Value::Number(n.into()))
        }
        "dynamicWorkflows.defaultLeader" => {
            validate_default_leader_value(value)?;
            Ok(serde_json::Value::String(value.to_string()))
        }
        "dynamicWorkflows.agentsToModels" => {
            // A map value can't be expressed as a `config set` string; without
            // this rejection the coerced string would fail RepoConfig
            // deserialization and the set would silently write nothing.
            Err(
                "dynamicWorkflows.agentsToModels cannot be set with `config set`; \
                 edit .awman/config.json directly"
                    .to_string(),
            )
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

/// Validate a `dynamicWorkflows.defaultLeader` value for `awman config set`
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

/// Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    // Row 0: dp[0][j] = j for j in 0..=n
    let first_row: Vec<usize> = (0..=n).collect();
    let mut dp: Vec<Vec<usize>> = std::iter::once(first_row)
        .chain((1..=m).map(|i| {
            let mut row = vec![0usize; n + 1];
            row[0] = i;
            row
        }))
        .collect();
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1]
            } else {
                1 + dp[i - 1][j - 1].min(dp[i - 1][j]).min(dp[i][j - 1])
            };
        }
    }
    dp[m][n]
}

/// Return candidates with levenshtein distance <= 3, sorted by distance ascending.
fn levenshtein_suggestions<'a>(input: &str, candidates: &[&'a str]) -> Vec<&'a str> {
    let mut scored: Vec<(usize, &'a str)> = candidates
        .iter()
        .filter_map(|c| {
            let dist = levenshtein(input, c);
            if dist <= 3 {
                Some((dist, *c))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by_key(|(d, _)| *d);
    scored.into_iter().map(|(_, c)| c).collect()
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

#[derive(Debug, Clone)]
pub enum ConfigSubcommand {
    Show(ConfigShowFlags),
    Get(ConfigGetFlags),
    Set(ConfigSetFlags),
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigShowOutcome {
    pub global: serde_json::Value,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldKind {
    Bool,
    Number,
    /// Fixed enum (e.g. agent name); the `set` validator rejects values
    /// outside the documented set.
    Enum,
    String,
}

/// Map a known config field name to its `ConfigFieldKind`. Mirrors the
/// schema in `RepoConfig` / `GlobalConfig`. Unknown fields default to
/// `String` (callers should reject them before reaching this function).
fn config_field_kind(name: &str) -> ConfigFieldKind {
    match name {
        "agent" | "default_agent" => ConfigFieldKind::Enum,
        "auto_agent_auth_accepted" | "api.background" => ConfigFieldKind::Bool,
        "terminal_scrollback_lines"
        | "agentStuckTimeout"
        | "api.port"
        | "dynamicWorkflows.maxConcurrentSteps" => ConfigFieldKind::Number,
        _ => ConfigFieldKind::String,
    }
}

/// Fields whose value is computed by awman itself and cannot be set by the
/// user via `awman config set`. Surfaced with `(read-only)` in the table.
const READ_ONLY_FIELDS: &[&str] = &["auto_agent_auth_accepted"];

/// Whether a config field is read-only in the show/edit UI. The
/// `dynamicWorkflows.agentsToModels` map and its per-agent expansions are
/// read-only in the table (edited directly in `.awman/config.json`), while the
/// scalar dynamic-workflow fields remain inline-editable (WI-0095).
fn is_read_only_field(name: &str) -> bool {
    READ_ONLY_FIELDS.contains(&name) || name.starts_with("dynamicWorkflows.agentsToModels")
}

/// Render an `agentsToModels` model-list JSON value as a comma-separated string
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

const SENSITIVE_FIELDS: &[&str] = &["remote.defaultAPIKey"];

fn mask_sensitive(field: &str, value: Option<String>) -> Option<String> {
    if !SENSITIVE_FIELDS.contains(&field) {
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

pub fn collect_config_rows(
    global: &serde_json::Value,
    repo: &serde_json::Value,
) -> Vec<ConfigFieldRow> {
    let mut rows: Vec<ConfigFieldRow> = Vec::new();
    for (name, _scope) in VALID_CONFIG_FIELDS {
        // The agentsToModels map is expanded into one row per agent below rather
        // than shown as a single unreadable JSON blob (WI-0095).
        if *name == "dynamicWorkflows.agentsToModels" {
            continue;
        }
        let g = config_field_value(global, name);
        let r = config_field_value(repo, name);
        let effective = r.clone().or_else(|| g.clone());
        rows.push(ConfigFieldRow {
            field: (*name).to_string(),
            global_value: mask_sensitive(name, g),
            repo_value: mask_sensitive(name, r),
            effective_value: mask_sensitive(name, effective),
            kind: config_field_kind(name),
            read_only: is_read_only_field(name),
        });
    }
    // Flatten dynamicWorkflows.agentsToModels into one read-only, repo-only row
    // per agent, keyed by `dynamicWorkflows.agentsToModels.<agentName>`. Keys are
    // sorted for deterministic ordering.
    if let Some(map) = repo
        .get("dynamicWorkflows")
        .and_then(|dw| dw.get("agentsToModels"))
        .and_then(|m| m.as_object())
    {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            let value = map.get(key).map(render_models_value);
            rows.push(ConfigFieldRow {
                field: format!("dynamicWorkflows.agentsToModels.{key}"),
                global_value: None,
                repo_value: value.clone(),
                effective_value: value,
                kind: ConfigFieldKind::String,
                read_only: true,
            });
        }
    }
    rows
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigGetOutcome {
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub effective_value: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSetOutcome {
    pub field: String,
    pub value: String,
    pub scope: String,
}

/// A user edit returned from the config show dialog.
#[derive(Debug, Clone)]
pub struct ConfigEditRequest {
    pub field: String,
    pub value: String,
    pub global: bool,
}

pub trait ConfigCommandFrontend: UserMessageSink + Send + Sync {
    /// Present the config table to the user and block until they either
    /// dismiss the dialog or edit a value. Returns `Ok(None)` on dismiss,
    /// `Ok(Some(edit))` when the user changes a field.
    fn present_config_table(
        &mut self,
        rows: &[ConfigFieldRow],
    ) -> Result<Option<ConfigEditRequest>, CommandError>;
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
}

#[async_trait]
impl Command for ConfigCommand {
    type Frontend = Box<dyn ConfigCommandFrontend>;
    type Outcome = ConfigOutcome;

    async fn run_with_frontend(
        self,
        mut frontend: Self::Frontend,
    ) -> Result<Self::Outcome, CommandError> {
        let _ = self.engines;
        let session = self.session;
        let names = valid_field_names();
        let outcome = match self.sub {
            ConfigSubcommand::Show(_) => {
                let mut session = session;
                loop {
                    let global = serde_json::to_value(session.global_config())
                        .unwrap_or(serde_json::Value::Null);
                    let repo = serde_json::to_value(session.repo_config())
                        .unwrap_or(serde_json::Value::Null);
                    let rows = collect_config_rows(&global, &repo);

                    match frontend.present_config_table(&rows)? {
                        None => {
                            let global = serde_json::to_value(session.global_config())
                                .unwrap_or(serde_json::Value::Null);
                            let repo = serde_json::to_value(session.repo_config())
                                .unwrap_or(serde_json::Value::Null);
                            let rows = collect_config_rows(&global, &repo);
                            break ConfigOutcome::Show(ConfigShowOutcome { global, repo, rows });
                        }
                        Some(edit) => {
                            let coerced = match validate_and_coerce(&edit.field, &edit.value) {
                                Ok(v) => v,
                                Err(reason) => {
                                    frontend.write_message(crate::data::message::UserMessage {
                                        level: crate::data::message::MessageLevel::Warning,
                                        text: format!("Invalid value: {reason}"),
                                    });
                                    continue;
                                }
                            };
                            if edit.global {
                                let mut cfg = session.global_config().clone();
                                let mut json = serde_json::to_value(&cfg).unwrap_or_default();
                                set_config_field(&mut json, &edit.field, coerced);
                                if let Ok(updated) = serde_json::from_value(json) {
                                    cfg = updated;
                                    let _ = cfg.save();
                                }
                            } else {
                                let mut cfg = session.repo_config().clone();
                                let mut json = serde_json::to_value(&cfg).unwrap_or_default();
                                set_config_field(&mut json, &edit.field, coerced);
                                if let Ok(updated) = serde_json::from_value(json) {
                                    cfg = updated;
                                    let _ = cfg.save(session.git_root());
                                }
                            }
                            session = {
                                let wd = session.working_dir().to_path_buf();
                                let gr = session.git_root().to_path_buf();
                                crate::data::session::Session::open_at_git_root(
                                    wd,
                                    gr,
                                    crate::data::session::SessionOpenOptions::default(),
                                )
                                .map_err(CommandError::from)?
                            };
                        }
                    }
                }
            }
            ConfigSubcommand::Get(f) => {
                // Validate field name.
                if let Some(msg) = removed_field_message(&f.field) {
                    return Err(CommandError::Other(msg.to_string()));
                }
                if !names.contains(&f.field.as_str()) {
                    let suggestions = levenshtein_suggestions(&f.field, &names);
                    return Err(CommandError::UnknownConfigField {
                        name: f.field.clone(),
                        suggestions: if suggestions.is_empty() {
                            "(none)".to_string()
                        } else {
                            suggestions.join(", ")
                        },
                    });
                }
                let global_value = config_field_value(
                    &serde_json::to_value(session.global_config())
                        .unwrap_or(serde_json::Value::Null),
                    &f.field,
                );
                let repo_value = config_field_value(
                    &serde_json::to_value(session.repo_config()).unwrap_or(serde_json::Value::Null),
                    &f.field,
                );
                let effective_value = repo_value.clone().or_else(|| global_value.clone());
                ConfigOutcome::Get(ConfigGetOutcome {
                    field: f.field,
                    global_value,
                    repo_value,
                    effective_value,
                })
            }
            ConfigSubcommand::Set(f) => {
                // Validate field name.
                if let Some(msg) = removed_field_message(&f.field) {
                    return Err(CommandError::Other(msg.to_string()));
                }
                if !names.contains(&f.field.as_str()) {
                    let suggestions = levenshtein_suggestions(&f.field, &names);
                    return Err(CommandError::UnknownConfigField {
                        name: f.field.clone(),
                        suggestions: if suggestions.is_empty() {
                            "(none)".to_string()
                        } else {
                            suggestions.join(", ")
                        },
                    });
                }
                // Validate scope: enforce GlobalOnly / RepoOnly constraints.
                if let Some(scope) = field_scope(&f.field) {
                    if scope == FieldScope::GlobalOnly && !f.global {
                        return Err(CommandError::InvalidFlagValue {
                            command: vec!["config".into(), "set".into()],
                            flag: "global".into(),
                            reason: format!(
                                "field '{}' can only be set in global config; add --global",
                                f.field
                            ),
                        });
                    }
                    if scope == FieldScope::RepoOnly && f.global {
                        return Err(CommandError::InvalidFlagValue {
                            command: vec!["config".into(), "set".into()],
                            flag: "global".into(),
                            reason: format!(
                                "field '{}' can only be set in repo config; omit --global",
                                f.field
                            ),
                        });
                    }
                }
                // Validate and coerce the value per field type.
                let coerced = validate_and_coerce(&f.field, &f.value).map_err(|reason| {
                    CommandError::InvalidFlagValue {
                        command: vec!["config".into(), "set".into()],
                        flag: f.field.clone(),
                        reason,
                    }
                })?;
                if f.global {
                    let mut cfg = session.global_config().clone();
                    let mut json = serde_json::to_value(&cfg).unwrap_or_default();
                    set_config_field(&mut json, &f.field, coerced.clone());
                    if let Ok(updated) = serde_json::from_value(json) {
                        cfg = updated;
                        let _ = cfg.save();
                    }
                } else {
                    let mut cfg = session.repo_config().clone();
                    let mut json = serde_json::to_value(&cfg).unwrap_or_default();
                    set_config_field(&mut json, &f.field, coerced.clone());
                    if let Ok(updated) = serde_json::from_value(json) {
                        cfg = updated;
                        let _ = cfg.save(session.git_root());
                    }
                }
                ConfigOutcome::Set(ConfigSetOutcome {
                    field: f.field,
                    value: f.value,
                    scope: if f.global {
                        "global".into()
                    } else {
                        "repo".into()
                    },
                })
            }
        };
        frontend.replay_queued();
        Ok(outcome)
    }
}

/// Look up a JSON field value, supporting dot-notation (e.g. "work_items.dir").
fn config_field_value(json: &serde_json::Value, field: &str) -> Option<String> {
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = json;
    for part in &parts {
        current = current.get(*part)?;
    }
    Some(match current {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => return None,
        other => other.to_string(),
    })
}

/// Set a JSON field, supporting dot-notation for nested objects.
/// E.g. "work_items.dir" sets `json["work_items"]["dir"]`.
fn set_config_field(json: &mut serde_json::Value, field: &str, value: serde_json::Value) {
    let parts: Vec<&str> = field.split('.').collect();
    if parts.len() == 1 {
        // Top-level field
        if let serde_json::Value::Object(obj) = json {
            obj.insert(field.to_string(), value);
        }
    } else {
        // Navigate into nested objects, creating intermediate objects as needed.
        let mut current = json;
        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                // Last segment: insert the value.
                if let serde_json::Value::Object(obj) = current {
                    obj.insert(part.to_string(), value);
                }
                return;
            }
            // Intermediate segment: ensure a nested object exists.
            if !current.get(*part).map(|v| v.is_object()).unwrap_or(false) {
                if let serde_json::Value::Object(obj) = current {
                    obj.insert(
                        part.to_string(),
                        serde_json::Value::Object(serde_json::Map::new()),
                    );
                }
            }
            current = current.get_mut(*part).expect("just inserted nested object");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── validate_and_coerce ──────────────────────────────────────────────────

    #[test]
    fn validate_and_coerce_agent_valid() {
        let v = validate_and_coerce("agent", "claude").unwrap();
        assert_eq!(v, serde_json::Value::String("claude".into()));
    }

    #[test]
    fn validate_and_coerce_agent_invalid() {
        let err = validate_and_coerce("agent", "notareal").unwrap_err();
        assert!(err.contains("not a known agent"));
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
        let v = validate_and_coerce("dynamicWorkflows.maxConcurrentSteps", "3").unwrap();
        assert_eq!(v, serde_json::json!(3u64));
    }

    #[test]
    fn validate_and_coerce_max_concurrent_steps_rejects_zero() {
        let err = validate_and_coerce("dynamicWorkflows.maxConcurrentSteps", "0").unwrap_err();
        assert!(
            err.contains("must be >= 1"),
            "zero must be rejected with the >= 1 message, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_default_leader_valid_shape() {
        let v = validate_and_coerce("dynamicWorkflows.defaultLeader", "claude::claude-opus-4-8")
            .unwrap();
        assert_eq!(
            v,
            serde_json::Value::String("claude::claude-opus-4-8".into())
        );
    }

    #[test]
    fn validate_and_coerce_default_leader_invalid_shape() {
        let err = validate_and_coerce("dynamicWorkflows.defaultLeader", "claude").unwrap_err();
        assert!(
            err.contains("expected agent::model"),
            "malformed leader must be rejected, got: {err}"
        );
    }

    #[test]
    fn validate_and_coerce_rejects_agents_to_models_map() {
        // The map field is valid for `config get`/`config show` but cannot be
        // written through `config set`; without this rejection the set would
        // silently no-op (string value fails RepoConfig deserialization).
        let err = validate_and_coerce("dynamicWorkflows.agentsToModels", "claude:foo").unwrap_err();
        assert!(
            err.contains("edit .awman/config.json directly"),
            "agentsToModels set must point at direct file editing, got: {err}"
        );
    }

    // ── collect_config_rows: dynamicWorkflows (WI-0095) ─────────────────────

    #[test]
    fn collect_config_rows_includes_dynamic_workflows_scalar_fields() {
        let global = serde_json::json!({});
        let repo = serde_json::json!({
            "dynamicWorkflows": {
                "defaultLeader": "claude::claude-opus-4-8",
                "maxConcurrentSteps": 3
            }
        });
        let rows = collect_config_rows(&global, &repo);

        let leader_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.defaultLeader")
            .expect("flattened dynamicWorkflows.defaultLeader row must be present");
        assert_eq!(
            leader_row.repo_value.as_deref(),
            Some("claude::claude-opus-4-8")
        );
        assert!(!leader_row.read_only, "defaultLeader stays inline-editable");

        let steps_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.maxConcurrentSteps")
            .expect("flattened dynamicWorkflows.maxConcurrentSteps row must be present");
        assert_eq!(steps_row.repo_value.as_deref(), Some("3"));
        assert!(
            !steps_row.read_only,
            "maxConcurrentSteps stays inline-editable"
        );
    }

    #[test]
    fn collect_config_rows_expands_agents_to_models_into_per_agent_rows() {
        let global = serde_json::json!({});
        let repo = serde_json::json!({
            "dynamicWorkflows": {
                "agentsToModels": {
                    "claude": ["claude-opus-4-8", "claude-sonnet-4-6"],
                    "codex": ["codex-mini-latest"]
                }
            }
        });
        let rows = collect_config_rows(&global, &repo);

        // The raw blob row must not appear — only its per-agent expansion.
        assert!(
            !rows
                .iter()
                .any(|r| r.field == "dynamicWorkflows.agentsToModels"),
            "the unexpanded agentsToModels blob row must not appear: {rows:?}"
        );

        let claude_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.agentsToModels.claude")
            .expect("per-agent row for claude must be present");
        assert_eq!(
            claude_row.repo_value.as_deref(),
            Some("claude-opus-4-8, claude-sonnet-4-6")
        );
        assert!(
            claude_row.read_only,
            "agentsToModels per-agent rows must be read-only"
        );

        let codex_row = rows
            .iter()
            .find(|r| r.field == "dynamicWorkflows.agentsToModels.codex")
            .expect("per-agent row for codex must be present");
        assert_eq!(codex_row.repo_value.as_deref(), Some("codex-mini-latest"));
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
        assert_eq!(field_scope("runtime"), Some(FieldScope::GlobalOnly));
    }

    #[test]
    fn field_scope_repo_only() {
        assert_eq!(field_scope("work_items.dir"), Some(FieldScope::RepoOnly));
    }

    #[test]
    fn field_scope_both() {
        assert_eq!(field_scope("agent"), Some(FieldScope::Both));
    }

    // ── levenshtein ───────────────────────────────────────────────────────────

    #[test]
    fn levenshtein_identical_strings_is_zero() {
        assert_eq!(levenshtein("agent", "agent"), 0);
    }

    #[test]
    fn levenshtein_empty_string_is_length_of_other() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn levenshtein_one_substitution() {
        assert_eq!(levenshtein("cat", "cut"), 1);
    }

    #[test]
    fn levenshtein_one_insertion() {
        assert_eq!(levenshtein("agent", "agents"), 1);
    }

    #[test]
    fn levenshtein_one_deletion() {
        assert_eq!(levenshtein("agents", "agent"), 1);
    }

    // ── levenshtein_suggestions ───────────────────────────────────────────────

    #[test]
    fn levenshtein_suggestions_finds_close_match() {
        let names = valid_field_names();
        let result = levenshtein_suggestions("agnet", &names);
        // "agnet" is distance 2 from "agent" (two transpositions); should appear.
        assert!(
            result.contains(&"agent"),
            "suggestions must contain 'agent' for input 'agnet': {result:?}"
        );
    }

    #[test]
    fn levenshtein_suggestions_empty_when_no_close_match() {
        let names = valid_field_names();
        let result = levenshtein_suggestions("zzzzzzzzzzz", &names);
        assert!(
            result.is_empty(),
            "suggestions must be empty for very distant input"
        );
    }

    #[test]
    fn levenshtein_suggestions_sorted_by_distance() {
        let names = valid_field_names();
        // "runtim" is distance 1 from "runtime" and distance 2+ from all others.
        let result = levenshtein_suggestions("runtim", &names);
        if result.len() >= 2 {
            // First result must be "runtime" (closest match).
            assert_eq!(
                result[0], "runtime",
                "closest match must be first: {result:?}"
            );
        }
    }
}
