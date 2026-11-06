//! `ConfigCommand` — `awman config [show] | get | set | unset`.
//!
//! Every key, its type, scope, sensitivity and whether a running daemon
//! applies it live come from the key table (`data::config::fields`). Old keys
//! are unknown keys; the key table's moved-to hint says where they went.
//!
//! - `show`: one row per key in table order (map entries under their map's
//!   header), with the global, repo and effective value, the layer the
//!   effective value comes from, and the type; sensitive values masked. Then
//!   a read-only `state` section naming the per-repo `state.json`.
//! - `get`: the same row for one key.
//! - `set` / `unset`: write one file — chosen by the key's scope unless
//!   `--global`, `--repo` or `--task <name>` names one — print the file
//!   written, and tell a running daemon to reload when the key is live.

mod live;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use crate::command::commands::Command;
use crate::command::dispatch::BuildContext;
use crate::command::error::CommandError;
use crate::data::config::config_json::{config_field_value, remove_config_field, set_config_field};
use crate::data::config::effective::{mask, Layer};
use crate::data::config::fields::{self, ConfigKey, KeyError, KeyScope, ValueKind, CONFIG_KEYS};
use crate::data::config::file::{global_config_path, repo_config_path, ConfigFile, FileKind};
use crate::data::config::state::RepoState;
use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
use crate::data::session::Session;

pub use live::{DaemonReloader, LiveApply, LocalDaemons, ReloadReply, RunningDaemon};

/// A config row's editing behaviour, derived from its key and handed to
/// frontends as data (the TUI's config dialog never inspects key names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigRowKind {
    Scalar,
    /// The header of a user-named map (`agents.flags`); members are added as
    /// `<child_prefix><name>`.
    MapHeader {
        child_prefix: &'static str,
    },
    ListHeader {
        child_prefix: &'static str,
    },
    MapEntry,
    ListEntry,
}

impl ConfigRowKind {
    pub fn child_prefix(self) -> Option<&'static str> {
        match self {
            Self::MapHeader { child_prefix } | Self::ListHeader { child_prefix } => {
                Some(child_prefix)
            }
            _ => None,
        }
    }

    pub fn is_header(self) -> bool {
        self.child_prefix().is_some()
    }

    pub fn member_shape(self) -> Option<ConfigRowKind> {
        match self {
            Self::MapHeader { .. } => Some(Self::MapEntry),
            Self::ListHeader { .. } => Some(Self::ListEntry),
            _ => None,
        }
    }
}

/// What kind of value a row holds, for rendering and editing widgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldKind {
    Bool,
    Number,
    Enum,
    String,
}

impl ConfigFieldKind {
    fn of(kind: ValueKind) -> Self {
        match kind {
            ValueKind::Bool => Self::Bool,
            ValueKind::Int { .. } => Self::Number,
            ValueKind::Enum(_) => Self::Enum,
            _ => Self::String,
        }
    }
}

/// Which file `config set` / `unset` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigTarget {
    /// Chosen by the key's scope: global keys → the global file; repo and
    /// task keys → the repo file inside a Git repository, else global.
    Auto,
    Global,
    Repo,
    Task(String),
}

impl ConfigTarget {
    fn from_flags(ctx: &BuildContext) -> Self {
        if ctx.flags.bool("global") {
            Self::Global
        } else if ctx.flags.bool("repo") {
            Self::Repo
        } else if let Some(task) = ctx.flags.string("task") {
            Self::Task(task)
        } else {
            Self::Auto
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigShowFlags {
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigGetFlags {
    pub field: String,
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigSetFlags {
    pub field: String,
    pub value: String,
    pub target: ConfigTarget,
}

#[derive(Debug, Clone)]
pub struct ConfigUnsetFlags {
    pub field: String,
    pub target: ConfigTarget,
}

#[derive(Debug, Clone)]
pub enum ConfigSubcommand {
    Show(ConfigShowFlags),
    Get(ConfigGetFlags),
    Set(ConfigSetFlags),
    Unset(ConfigUnsetFlags),
}

/// The per-repo state `config show` prints under its own heading. It is
/// state, not config: nothing here can be set with `config set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigStateSection {
    /// The `state.json` file the values come from.
    pub path: String,
    #[serde(rename = "autoAgentAuthAccepted")]
    pub auto_agent_auth_accepted: Option<bool>,
}

/// `runtime.kind` maturity, stated by `config show` (never in the value).
pub const RUNTIME_MATURITY_NOTE: &str =
    "runtime.kind: docker and apple are stable; sbx and builtin are experimental.";

#[derive(Debug, Clone, Serialize)]
pub struct ConfigShowOutcome {
    pub rows: Vec<ConfigFieldRow>,
    /// `None` outside a Git repository (state is per repository).
    pub state: Option<ConfigStateSection>,
    pub notes: Vec<String>,
}

/// One row of `config show` / the TUI config dialog.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigFieldRow {
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub task_value: Option<String>,
    pub effective_value: Option<String>,
    /// The layer the effective value comes from.
    pub source: Layer,
    /// The key table's type text (`duration`, `enum stdio|acp`, …).
    pub type_name: String,
    pub kind: ConfigFieldKind,
    pub sensitive: bool,
    pub read_only: bool,
    pub global_writable: bool,
    pub repo_writable: bool,
    pub value_hint: Option<String>,
    pub shape: ConfigRowKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigGetOutcome {
    pub field: String,
    pub global_value: Option<String>,
    pub repo_value: Option<String>,
    pub effective_value: Option<String>,
    pub source: Layer,
    pub type_name: String,
}

/// What `config set` / `unset` did.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigSetOutcome {
    pub field: String,
    /// The value written (masked for sensitive keys); `None` for `unset`.
    pub value: Option<String>,
    /// `global`, `repo` or `task`.
    pub scope: String,
    /// The file written.
    pub path: String,
    pub live: LiveApply,
}

/// A user edit returned from the config show dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEditRequest {
    pub field: String,
    pub value: String,
    pub global: bool,
}

impl ConfigEditRequest {
    pub fn to_field(field: impl Into<String>, value: impl Into<String>, global: bool) -> Self {
        Self {
            field: field.into(),
            value: value.into(),
            global,
        }
    }

    /// A new member, named `member`, of the map whose header has `shape`.
    pub fn new_member(
        shape: &ConfigRowKind,
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

/// An edit that failed validation or could not be written.
#[derive(Debug, Clone)]
pub struct ConfigEditRejection {
    pub field: String,
    pub value: String,
    pub global: bool,
    pub reason: String,
}

pub trait ConfigCommandFrontend: UserMessageSink + Send + Sync {
    fn present_config_table(
        &mut self,
        rows: &[ConfigFieldRow],
        rejected: Option<&ConfigEditRejection>,
    ) -> Result<Option<ConfigEditRequest>, CommandError>;
}

pub struct ConfigCommand {
    sub: ConfigSubcommand,
    session: Session,
    reloader: Arc<dyn DaemonReloader>,
}

impl ConfigCommand {
    pub fn new(sub: ConfigSubcommand, session: Session) -> Self {
        Self {
            sub,
            session,
            reloader: Arc::new(LocalDaemons),
        }
    }

    /// Replace how running daemons are found and asked to reload (tests).
    pub fn with_reloader(mut self, reloader: Arc<dyn DaemonReloader>) -> Self {
        self.reloader = reloader;
        self
    }

    pub fn from_input(ctx: &BuildContext) -> Result<Self, CommandError> {
        let sub = match ctx.caller.leaf() {
            // Bare `awman config` is `awman config show`.
            "config" | "show" => ConfigSubcommand::Show(ConfigShowFlags {
                json: ctx.flags.bool("json"),
            }),
            "get" => ConfigSubcommand::Get(ConfigGetFlags {
                field: ctx.args.require("key")?,
                json: ctx.flags.bool("json"),
            }),
            "set" => ConfigSubcommand::Set(ConfigSetFlags {
                field: ctx.args.require("key")?,
                value: ctx.args.require("value")?,
                target: ConfigTarget::from_flags(ctx),
            }),
            "unset" => ConfigSubcommand::Unset(ConfigUnsetFlags {
                field: ctx.args.require("key")?,
                target: ConfigTarget::from_flags(ctx),
            }),
            _ => return Err(CommandError::unknown_command(&ctx.path())),
        };
        Ok(Self::new(sub, ctx.session.clone()))
    }

    pub fn subcommand(&self) -> &ConfigSubcommand {
        &self.sub
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ConfigOutcome {
    Show(ConfigShowOutcome),
    Get(ConfigGetOutcome),
    Set(ConfigSetOutcome),
    Unset(ConfigSetOutcome),
}

fn key_error(error: KeyError) -> CommandError {
    CommandError::Other(error.to_string())
}

/// Display text of the value at `path` in `doc`, masked for sensitive keys.
fn display(doc: &serde_json::Value, path: &str, key: &ConfigKey) -> Option<String> {
    let value = config_field_value(doc, path)?;
    Some(if key.sensitive { mask(&value) } else { value })
}

fn writability(key: &ConfigKey) -> (bool, bool) {
    (
        key.kind != ValueKind::Object,
        key.kind != ValueKind::Object && key.scope >= KeyScope::Repo,
    )
}

fn value_hint(key: &ConfigKey) -> Option<String> {
    Some(match key.kind {
        ValueKind::Enum(values) => values.join(" | "),
        ValueKind::Bool => "true | false".into(),
        ValueKind::Duration => "e.g. 30s, 15m".into(),
        ValueKind::Size => "e.g. 512MiB, 4GiB".into(),
        ValueKind::StringList | ValueKind::IntList => "comma-separated list".into(),
        ValueKind::StringMap => "k=v,k2=v2".into(),
        ValueKind::Object => "edit in the config file".into(),
        _ => return None,
    })
}

/// The documents a row reads values from.
pub struct ConfigDocs<'a> {
    pub global: &'a serde_json::Value,
    pub repo: &'a serde_json::Value,
    /// Already masked.
    pub effective: &'a serde_json::Value,
    /// Layer of the effective value at a concrete path.
    pub source: &'a dyn Fn(&str) -> Layer,
}

fn row(
    path: String,
    key: &ConfigKey,
    docs: &ConfigDocs<'_>,
    shape: ConfigRowKind,
) -> ConfigFieldRow {
    let (global_writable, repo_writable) = writability(key);
    ConfigFieldRow {
        global_value: display(docs.global, &path, key),
        repo_value: display(docs.repo, &path, key),
        task_value: None,
        // `effective` is already masked (`EffectiveConfig::masked_value`).
        effective_value: config_field_value(docs.effective, &path),
        source: (docs.source)(&path),
        type_name: key.kind.describe(),
        kind: ConfigFieldKind::of(key.kind),
        sensitive: key.sensitive,
        read_only: key.kind == ValueKind::Object,
        global_writable,
        repo_writable,
        value_hint: value_hint(key),
        shape,
        field: path,
    }
}

/// The names bound at a map path in any of `docs`.
fn names_at(docs: &[&serde_json::Value], map_path: &str) -> Vec<String> {
    let mut names = Vec::new();
    for doc in docs {
        let node = map_path.split('.').try_fold(*doc, |v, part| v.get(part));
        for name in node
            .and_then(|v| v.as_object())
            .into_iter()
            .flat_map(|m| m.keys())
        {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names
}

/// One row per key, in key-table order. A map whose members are leaves
/// (`agents.flags.*`) gets a header row and one row per name present in any
/// file; a map of objects (`workServers.*.url`) gets one row per name and leaf.
pub fn collect_config_rows(docs: &ConfigDocs<'_>) -> Vec<ConfigFieldRow> {
    let mut rows = Vec::new();
    for key in CONFIG_KEYS {
        let Some(star) = key.path.find('*') else {
            rows.push(row(key.path.to_string(), key, docs, ConfigRowKind::Scalar));
            continue;
        };
        let map_path = key.path[..star].trim_end_matches('.');
        let names = names_at(&[docs.global, docs.repo, docs.effective], map_path);
        if key.path.ends_with(".*") {
            let prefix: &'static str = &key.path[..key.path.len() - 1];
            let mut header = row(
                map_path.to_string(),
                key,
                docs,
                ConfigRowKind::MapHeader {
                    child_prefix: prefix,
                },
            );
            header.global_value = None;
            header.repo_value = None;
            header.effective_value = None;
            header.kind = ConfigFieldKind::String;
            header.read_only = true;
            rows.push(header);
            for name in names {
                rows.push(row(
                    format!("{prefix}{name}"),
                    key,
                    docs,
                    ConfigRowKind::MapEntry,
                ));
            }
        } else {
            for name in names {
                let path = key.path.replacen('*', &name, 1);
                rows.push(row(path, key, docs, ConfigRowKind::Scalar));
            }
        }
    }
    rows
}

/// The rows of the session's merged config.
fn session_rows(session: &Session) -> Vec<ConfigFieldRow> {
    let config = session.effective_config();
    let layers = config.layers();
    let (global, repo) = (layers.global.to_value(), layers.repo.to_value());
    let effective = config.masked_value();
    let source = |path: &str| config.source_of(path);
    collect_config_rows(&ConfigDocs {
        global: &global,
        repo: &repo,
        effective: &effective,
        source: &source,
    })
}

/// The `state` section: the per-repo `state.json` and what it holds.
fn state_section(session: &Session) -> Option<ConfigStateSection> {
    if !session.is_git_repo() {
        return None;
    }
    let data = crate::data::fs::DataPaths::from_env(session.env()).ok()?;
    let git_root = session.git_root();
    Some(ConfigStateSection {
        path: RepoState::path(&data, git_root).display().to_string(),
        auto_agent_auth_accepted: RepoState::load(&data, git_root)
            .unwrap_or_default()
            .auto_agent_auth_accepted,
    })
}

/// A key (or a whole named map entry, for `unset`) resolved to the row whose
/// scope decides the file.
fn resolve_for_edit(path: &str, allow_map_entry: bool) -> Result<&'static ConfigKey, CommandError> {
    match fields::resolve(path) {
        Ok(resolved) => Ok(resolved.key),
        Err(error @ KeyError::Unknown { .. }) if allow_map_entry => {
            // `unset workServers.foo` names a whole map entry.
            let pattern = fields::pattern_of(path).ok_or_else(|| key_error(error.clone()))?;
            if !pattern.ends_with('*') {
                return Err(key_error(error));
            }
            let prefix = format!("{pattern}.");
            CONFIG_KEYS
                .iter()
                .find(|k| k.path.starts_with(&prefix))
                .ok_or_else(|| key_error(error))
        }
        Err(error) => Err(key_error(error)),
    }
}

/// Which file a `config set` / `unset` writes, and where it is.
fn target_file(
    session: &Session,
    key: &ConfigKey,
    path: &str,
    target: &ConfigTarget,
) -> Result<(FileKind, PathBuf), CommandError> {
    let kind = match target {
        ConfigTarget::Global => FileKind::Global,
        ConfigTarget::Repo if !session.is_git_repo() => {
            return Err(CommandError::Other(
                "--repo needs a Git repository; run it inside one or use --global".into(),
            ))
        }
        ConfigTarget::Repo => FileKind::Repo,
        ConfigTarget::Task(_) => FileKind::Task,
        ConfigTarget::Auto if key.scope == KeyScope::Global || !session.is_git_repo() => {
            FileKind::Global
        }
        ConfigTarget::Auto => FileKind::Repo,
    };
    if !kind.admits(key.scope) {
        return Err(key_error(KeyError::Scope {
            path: path.to_string(),
            allowed: key.scope.allowed_files(),
            found: kind.as_str(),
        }));
    }
    let file = match (kind, target) {
        (FileKind::Global, _) => global_config_path(session.env())?,
        (FileKind::Task, ConfigTarget::Task(name)) => {
            let squad = crate::data::fs::squad_paths::SquadPaths::from_env(session.env())?;
            let file = crate::data::config::file::task_config_path(&squad, name)?;
            if !file.parent().is_some_and(std::path::Path::exists) {
                return Err(CommandError::Other(format!("no squad task named `{name}`")));
            }
            file
        }
        _ => repo_config_path(session.git_root()),
    };
    Ok((kind, file))
}

/// Validate and write (or, with `raw = None`, remove) one key. Returns the
/// file kind and path written.
fn write_key(
    session: &Session,
    path: &str,
    raw: Option<&str>,
    target: &ConfigTarget,
) -> Result<(FileKind, PathBuf, &'static ConfigKey), CommandError> {
    let key = resolve_for_edit(path, raw.is_none())?;
    let value = match raw {
        Some(raw) => Some(fields::parse_input(key, raw).map_err(key_error)?),
        None => None,
    };
    let (kind, file) = target_file(session, key, path, target)?;
    let mut doc = ConfigFile::load(&file, kind)?.to_value();
    match value {
        Some(value) if !value.is_null() => set_config_field(&mut doc, path, value),
        // `set key null` and `unset key` both remove the key from the file.
        _ => {
            remove_config_field(&mut doc, path);
        }
    }
    ConfigFile::from_value(doc, kind)?.save(&file, kind)?;
    Ok((kind, file, key))
}

fn reopen(session: &Session) -> Result<Session, CommandError> {
    let opts = crate::data::session::SessionOpenOptions {
        env: Some(session.env().clone()),
        ..Default::default()
    };
    let mut reopened = if session.is_git_repo() {
        Session::open_at_git_root(
            session.working_dir().to_path_buf(),
            session.git_root().to_path_buf(),
            opts,
        )
    } else {
        Session::open_at_directory_root(session.git_root().to_path_buf(), opts)
    }
    .map_err(CommandError::from)?;
    // Keep this invocation's `--set` layer.
    reopened.add_overrides(session.effective_config().layers().set.clone())?;
    Ok(reopened)
}

fn scope_name(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Global => "global",
        FileKind::Repo => "repo",
        FileKind::Task => "task",
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
        let reloader = self.reloader;
        let outcome = match self.sub {
            ConfigSubcommand::Show(_) => {
                let mut session = session;
                let mut rejected: Option<ConfigEditRejection> = None;
                loop {
                    let rows = session_rows(&session);
                    match frontend.present_config_table(&rows, rejected.take().as_ref())? {
                        None => {
                            break ConfigOutcome::Show(ConfigShowOutcome {
                                rows,
                                state: state_section(&session),
                                notes: vec![RUNTIME_MATURITY_NOTE.to_string()],
                            })
                        }
                        Some(edit) => {
                            let target = if edit.global {
                                ConfigTarget::Global
                            } else {
                                ConfigTarget::Auto
                            };
                            match write_key(&session, &edit.field, Some(&edit.value), &target) {
                                Ok(_) => session = reopen(&session)?,
                                Err(error) => {
                                    let reason = error.to_string();
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
                                }
                            }
                        }
                    }
                }
            }
            ConfigSubcommand::Get(f) => {
                let key = fields::resolve(&f.field).map_err(key_error)?.key;
                let config = session.effective_config();
                let layers = config.layers();
                let (global, repo) = (layers.global.to_value(), layers.repo.to_value());
                let effective = config.masked_value();
                ConfigOutcome::Get(ConfigGetOutcome {
                    global_value: display(&global, &f.field, key),
                    repo_value: display(&repo, &f.field, key),
                    effective_value: config_field_value(&effective, &f.field),
                    source: config.source_of(&f.field),
                    type_name: key.kind.describe(),
                    field: f.field,
                })
            }
            ConfigSubcommand::Set(f) => {
                let (kind, file, key) = write_key(&session, &f.field, Some(&f.value), &f.target)?;
                let live = live::apply(reloader.as_ref(), &session, key, &f.field);
                ConfigOutcome::Set(ConfigSetOutcome {
                    value: Some(if key.sensitive {
                        mask(&f.value)
                    } else {
                        f.value
                    }),
                    field: f.field,
                    scope: scope_name(kind).into(),
                    path: file.display().to_string(),
                    live,
                })
            }
            ConfigSubcommand::Unset(f) => {
                let (kind, file, key) = write_key(&session, &f.field, None, &f.target)?;
                let live = live::apply(reloader.as_ref(), &session, key, &f.field);
                ConfigOutcome::Unset(ConfigSetOutcome {
                    value: None,
                    field: f.field,
                    scope: scope_name(kind).into(),
                    path: file.display().to_string(),
                    live,
                })
            }
        };
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests;
