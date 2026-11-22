//! Definition-selected sanitized settings and history staging. Guest state is
//! untrusted input; only allowlisted settings and provider history are retained.
use super::session_seed::copy_profile_tree;
use crate::data::fs::private_files::{create_private_dir, write_private_json_atomic};
use crate::data::settings_profile::NativeSessionId;
use crate::engine::error::EngineError;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRoot {
    pub home_relative: String,
    pub file: bool,
    pub env_var: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedSanitizer {
    ClaudeConfig,
    ClaudeSettings,
    CodexToml,
    OpenCodeJson,
    CursorCliConfig,
    CopilotConfig,
    ClineSettings,
    AntigravitySettings,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedRule {
    pub path: String,
    pub sanitizer: SeedSanitizer,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileStateSpec {
    pub identity: HistoryIdentity,
    pub allow_leading_dash: bool,
    pub roots: Vec<ProfileRoot>,
    pub settings: Vec<SeedRule>,
    pub history: Vec<String>,
    pub exclude: Vec<String>,
    pub exclude_basename_prefixes: Vec<(String, String)>,
    pub database_history: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryIdentity {
    Claude,
    Codex,
    OpenCode,
    Cursor,
    Copilot,
    Cline,
    Antigravity,
}
#[derive(Debug, Clone)]
pub struct ProfileRunState {
    pub run_dir: PathBuf,
    pub execution_id: crate::data::ids::ExecutionId,
}
#[derive(Debug)]
pub struct ExportedSnapshot {
    pub sessions: Vec<NativeSessionId>,
    pub history_unavailable: Option<String>,
}
impl ProfileStateSpec {
    /// Stable identity for the provider-owned paths and schemas in this state
    /// contract. Profile manifests use it to refuse imports after an adapter
    /// or provider layout changes under the same custom agent name.
    pub fn layout_key(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{}",
            self.identity,
            self.roots,
            self.settings,
            self.history,
            self.exclude,
            self.database_history
        )
    }

    /// Remove excluded provider state from a stopped run tree before it can
    /// remain behind in a closing profile. The tree is never traversed through
    /// symlinks by remove_tree.
    pub fn purge_excluded(&self, run: &Path) -> Result<(), crate::data::error::DataError> {
        for path in &self.exclude {
            crate::data::fs::agent_profiles::remove_tree(&run.join(path))?;
        }
        for (root, prefix) in &self.exclude_basename_prefixes {
            let dir = run.join(root);
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(crate::data::error::DataError::io(&dir, error)),
            };
            for entry in entries {
                let entry =
                    entry.map_err(|error| crate::data::error::DataError::io(&dir, error))?;
                if entry.file_name().to_string_lossy().starts_with(prefix) {
                    crate::data::fs::agent_profiles::remove_tree(&entry.path())?;
                }
            }
        }
        Ok(())
    }
}
impl ProfileStateSpec {
    pub fn ensure_roots(&self, tree: &Path) -> Result<(), EngineError> {
        for root in &self.roots {
            let p = tree.join(&root.home_relative);
            if root.file {
                if let Some(parent) = p.parent() {
                    create_private_dir(parent)?;
                }
                if !p.exists() {
                    write_private_json_atomic(&p, &serde_json::json!({}), || Ok(()))?;
                }
            } else {
                create_private_dir(&p)?;
            }
        }
        Ok(())
    }
    fn excluded(&self, path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        // Definition exclusions run before opening an entry. Ordinary
        // transcript names such as tokenizer must retain their history.
        name.ends_with(".sock")
            || name.ends_with(".lock")
            || name.ends_with(".pid")
            || self.exclude_basename_prefixes.iter().any(|(root, prefix)| {
                path.starts_with(&format!("{root}/")) && name.starts_with(prefix)
            })
            || self
                .exclude
                .iter()
                .any(|p| path == p || path.starts_with(&format!("{p}/")))
    }

    fn selected(&self, p: &Path) -> bool {
        let path = p.to_string_lossy();
        !self.excluded(&path)
            && self
                .settings
                .iter()
                .map(|s| s.path.as_str())
                .chain(self.history.iter().map(String::as_str))
                .any(|v| {
                    path == v
                        || path.starts_with(&format!("{v}/"))
                        || v.starts_with(&format!("{path}/"))
                })
    }
    pub fn import(&self, source: &Path, dest: &Path) -> Result<(), EngineError> {
        copy_profile_tree(source, dest, &|p| self.selected(p), &|p, b| {
            self.transform(p, b)
        })?;
        self.ensure_roots(dest)
    }
    fn transform(&self, p: &Path, bytes: Vec<u8>) -> Result<Vec<u8>, EngineError> {
        if let Some(rule) = self.settings.iter().find(|r| Path::new(&r.path) == p) {
            sanitize(&rule.sanitizer, &bytes)
        } else {
            Ok(bytes)
        }
    }
    pub fn seed(
        &self,
        home: Option<&Path>,
        data_home: Option<&Path>,
        staging: &Path,
    ) -> Result<ExportedSnapshot, EngineError> {
        let tree = staging.join("tree");
        if let Some(home) = home {
            for rule in &self.settings {
                let source = home.join(&rule.path);
                if crate::engine::overlay::seed_overlaps_protected_root(&source, home, data_home) {
                    return Err(EngineError::Config(
                        "profile seed overlaps protected state".into(),
                    ));
                }
            }
            copy_profile_tree(
                home,
                &tree,
                &|p| {
                    let path = p.to_string_lossy();
                    self.settings
                        .iter()
                        .any(|r| r.path == path || r.path.starts_with(&format!("{path}/")))
                },
                &|p, b| self.transform(p, b),
            )?;
        }
        self.snapshot(&tree, staging)
    }
    pub fn export(&self, run: &Path, staging: &Path) -> Result<ExportedSnapshot, EngineError> {
        self.export_redacted(run, staging, None)
    }
    pub fn export_redacted(
        &self,
        run: &Path,
        staging: &Path,
        redaction: Option<&crate::engine::auth::redaction::RedactionSet>,
    ) -> Result<ExportedSnapshot, EngineError> {
        if redaction.is_some_and(|r| r.is_retired()) {
            return Err(EngineError::Config("output_incomplete".into()));
        }
        let tree = staging.join("tree");
        super::session_seed::copy_profile_tree_with_owner(
            run,
            &tree,
            &|p| self.selected(p),
            &|p, b| {
                let bytes = self.transform(p, b)?;
                match redaction {
                    Some(r) if r.is_retired() => {
                        Err(EngineError::Config("output_incomplete".into()))
                    }
                    Some(r) => Ok(r.redact_bytes(&bytes)),
                    None => Ok(bytes),
                }
            },
            false,
        )?;
        if redaction.is_some_and(|r| r.is_retired()) {
            return Err(EngineError::Config("output_incomplete".into()));
        }
        let mut snapshot = self.snapshot(&tree, staging)?;
        if run
            .join(".local/share/opencode/opencode.db")
            .symlink_metadata()
            .is_ok()
        {
            snapshot.history_unavailable =
                Some("OpenCode database history schema has not been qualified".into());
        }
        Ok(snapshot)
    }
    fn snapshot(&self, tree: &Path, staging: &Path) -> Result<ExportedSnapshot, EngineError> {
        let sessions = self.list_sessions(tree)?;
        let mut files = Vec::new();
        list_files(tree, tree, &mut files)?;
        files.sort();
        let bytes = files.iter().map(|(_, n)| *n).sum::<u64>();
        let sha = crate::data::fs::hash::sha256_hex(
            serde_json::to_string(&files).unwrap_or_default().as_bytes(),
        );
        write_private_json_atomic(
            &staging.join("snapshot.json"),
            &serde_json::json!({"schema":1,"created_at":crate::data::fs::agent_profiles::now(),"entries":files.len(),"bytes":bytes,"sha256":sha}),
            || Ok(()),
        )?;
        Ok(ExportedSnapshot {
            sessions,
            history_unavailable: self
                .database_history
                .then(|| "OpenCode database history schema has not been qualified".into()),
        })
    }
    pub fn list_sessions(&self, tree: &Path) -> Result<Vec<NativeSessionId>, EngineError> {
        let mut files = Vec::new();
        list_files(tree, tree, &mut files)?;
        let mut ids = Vec::new();
        for (path, _) in files {
            if !self
                .history
                .iter()
                .any(|r| path.starts_with(&format!("{r}/")) || path == *r)
            {
                continue;
            }
            let p = Path::new(&path);
            if self.identity == HistoryIdentity::Claude
                && (path == ".claude/history.jsonl"
                    || !path.starts_with(".claude/projects/")
                    || !path.ends_with(".jsonl"))
            {
                continue;
            }
            if self.identity == HistoryIdentity::OpenCode
                && !path.starts_with(".local/share/opencode/storage/session/")
            {
                continue;
            }
            if self.identity == HistoryIdentity::Cursor {
                if let Some(id) = cursor_chat_id(p) {
                    ids.push(id);
                }
                continue;
            }
            // Session ids come from provider-owned transcript records, never
            // terminal text. Validate every opaque id before exposing it.
            let file_path = tree.join(p);
            let file =
                std::fs::File::open(&file_path).map_err(|e| EngineError::io(&file_path, e))?;
            let reader = std::io::BufReader::new(file);
            let mut reader = std::io::Read::take(reader, 512 * 1024 * 1024 + 1);
            let mut line = Vec::new();
            let mut total = 0u64;
            loop {
                line.clear();
                use std::io::BufRead;
                let read = reader
                    .read_until(b'\n', &mut line)
                    .map_err(|e| EngineError::io(&file_path, e))?;
                if read == 0 {
                    break;
                }
                total = total.saturating_add(read as u64);
                if total > 512 * 1024 * 1024 {
                    return Err(EngineError::Config(
                        "profile history file exceeds 512 MiB".into(),
                    ));
                }
                if line.len() > 1024 * 1024 {
                    return Err(EngineError::Config(
                        "profile history record exceeds 1 MiB".into(),
                    ));
                }
                if let Ok(text) = std::str::from_utf8(&line) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                        collect_ids(self.identity, &value, &mut ids);
                    }
                }
            }
            if self.identity == HistoryIdentity::Cline {
                if let Ok(rel) = p.strip_prefix(".cline/data/tasks") {
                    if let Some(std::path::Component::Normal(id)) = rel.components().next() {
                        if let Some(id) = id
                            .to_str()
                            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
                            .and_then(|s| NativeSessionId::parse(s).ok())
                        {
                            ids.push(id);
                        }
                    }
                }
            }
            // Stores whose transcript filename/directory *is* the native id.
            for part in p.components() {
                let value = part.as_os_str().to_string_lossy();
                let stem = value
                    .strip_suffix(".jsonl")
                    .or_else(|| value.strip_suffix(".json"))
                    .unwrap_or(&value);
                let provider_session_path = match self.identity {
                    HistoryIdentity::Claude => canonical_uuid(stem),
                    HistoryIdentity::Codex => false,
                    HistoryIdentity::OpenCode => stem.starts_with("ses_"),
                    HistoryIdentity::Cursor => false,
                    HistoryIdentity::Copilot
                    | HistoryIdentity::Cline
                    | HistoryIdentity::Antigravity => false,
                };
                if provider_session_path {
                    if let Ok(id) = NativeSessionId::parse(stem) {
                        ids.push(id);
                    }
                }
                if let Some(suffix) = stem.strip_prefix("rollout-") {
                    if suffix.len() >= 36 {
                        let id = &suffix[suffix.len() - 36..];
                        if canonical_uuid(id) {
                            ids.push(NativeSessionId::parse(id).unwrap());
                        }
                    }
                }
            }
        }
        ids.sort();
        ids.dedup();
        if ids.len() > crate::data::fs::agent_profiles::MAX_NATIVE_SESSIONS {
            return Err(EngineError::Config(
                "native session record budget exceeded".into(),
            ));
        }
        Ok(ids)
    }
}
fn collect_ids(kind: HistoryIdentity, value: &serde_json::Value, out: &mut Vec<NativeSessionId>) {
    let keys: &[&str] = match kind {
        HistoryIdentity::Claude => &["sessionId"],
        HistoryIdentity::Codex => &[],
        HistoryIdentity::OpenCode => &["sessionID"],
        HistoryIdentity::Cursor => &["session_id"],
        HistoryIdentity::Copilot => &["sessionId", "session_id"],
        HistoryIdentity::Cline => &["taskId"],
        HistoryIdentity::Antigravity => &["conversationId", "conversation_id"],
    };
    for key in keys {
        if let Some(id) = value
            .get(*key)
            .and_then(|v| v.as_str())
            .and_then(|s| NativeSessionId::parse(s).ok())
        {
            out.push(id)
        }
    }
    if kind == HistoryIdentity::Codex
        && value.get("type").and_then(|v| v.as_str()) == Some("session_meta")
    {
        if let Some(id) = value
            .pointer("/payload/id")
            .and_then(|v| v.as_str())
            .and_then(|s| NativeSessionId::parse(s).ok())
        {
            out.push(id)
        }
    }
}

fn canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.hyphenated().to_string() == value)
}

fn cursor_chat_id(path: &Path) -> Option<NativeSessionId> {
    let relative = path.strip_prefix(".cursor/chats").ok()?;
    let mut components = relative.components();
    components.next()?; // workspace hash
    let chat = components.next()?.as_os_str().to_str()?;
    canonical_uuid(chat)
        .then(|| NativeSessionId::parse(chat).ok())
        .flatten()
}

fn list_files(root: &Path, path: &Path, out: &mut Vec<(String, u64)>) -> Result<(), EngineError> {
    if !path.exists() {
        return Ok(());
    }
    let m = std::fs::symlink_metadata(path).map_err(|e| EngineError::io(path, e))?;
    if m.file_type().is_symlink() {
        return Err(EngineError::Config("unsafe profile history link".into()));
    }
    if m.is_dir() {
        for e in std::fs::read_dir(path).map_err(|e| EngineError::io(path, e))? {
            list_files(root, &e.map_err(|e| EngineError::io(path, e))?.path(), out)?;
        }
    } else if m.is_file() {
        out.push((
            path.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            m.len(),
        ));
    } else {
        return Err(EngineError::Config("unsafe profile history entry".into()));
    }
    if out.len() > 50_000 {
        return Err(EngineError::Config(
            "profile history exceeds entry budget".into(),
        ));
    }
    Ok(())
}
fn safe_endpoint(value: &serde_json::Value) -> bool {
    value.as_str().is_some_and(|s| {
        reqwest::Url::parse(s)
            .is_ok_and(|u| u.username().is_empty() && u.password().is_none() && u.query().is_none())
    })
}
pub fn sanitize(kind: &SeedSanitizer, bytes: &[u8]) -> Result<Vec<u8>, EngineError> {
    use serde_json::{json, Value};
    use SeedSanitizer::*;
    if *kind == CodexToml {
        let text = std::str::from_utf8(bytes).map_err(|_| {
            EngineError::Config(
                "invalid Codex profile settings; restrictive policy cannot be dropped".into(),
            )
        })?;
        let input: toml::Value = toml::from_str(text).map_err(|_| {
            EngineError::Config(
                "invalid Codex profile settings; restrictive policy cannot be dropped".into(),
            )
        })?;
        let mut out = toml::map::Map::new();
        for k in ["model", "model_reasoning_effort", "model_provider"] {
            if let Some(v) = input.get(k) {
                out.insert(k.into(), v.clone());
            }
        }
        for (k, allowed) in [
            ("sandbox_mode", &["read-only"][..]),
            ("approval_policy", &["untrusted", "on-request"][..]),
        ] {
            if let Some(v) = input.get(k) {
                if v.as_str().is_some_and(|s| allowed.contains(&s)) {
                    out.insert(k.into(), v.clone());
                }
            }
        }
        if let Some(providers) = input.get("model_providers").and_then(|v| v.as_table()) {
            let mut kept = toml::map::Map::new();
            for (id, p) in providers {
                let mut row = toml::map::Map::new();
                for k in ["name", "wire_api", "env_key", "base_url"] {
                    if let Some(v) = p.get(k) {
                        if k != "base_url" || safe_endpoint(&json!(v.as_str())) {
                            row.insert(k.into(), v.clone());
                        }
                    }
                }
                kept.insert(id.clone(), toml::Value::Table(row));
            }
            out.insert("model_providers".into(), toml::Value::Table(kept));
        }
        if let Some(theme) = input.get("tui").and_then(|v| v.get("theme")) {
            out.insert(
                "tui".into(),
                toml::Value::Table(
                    [(String::from("theme"), theme.clone())]
                        .into_iter()
                        .collect(),
                ),
            );
        }
        preserve_restrictions(
            &serde_json::to_value(&input)
                .map_err(|_| EngineError::Config("invalid restrictive profile policy".into()))?,
            &serde_json::to_value(toml::Value::Table(out.clone()))
                .map_err(|_| EngineError::Config("invalid sanitized profile policy".into()))?,
        )?;
        return toml::to_string(&toml::Value::Table(out))
            .map(String::into_bytes)
            .map_err(|e| EngineError::Config(e.to_string()));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| EngineError::Config("invalid profile settings encoding".into()))?;
    let input: Value = crate::data::jsonc::parse_tolerant(text).map_err(|_| {
        EngineError::Config("invalid profile settings; deny/ask policy cannot be dropped".into())
    })?;
    if !input.is_object() {
        return Err(EngineError::Config(
            "profile settings must be an object".into(),
        ));
    }
    let mut out = json!({});
    let keys: &[&str] = match kind {
        ClaudeConfig => &[
            "theme",
            "editorMode",
            "autoUpdates",
            "hasCompletedOnboarding",
        ],
        ClaudeSettings => &["model", "effortLevel", "outputStyle"],
        OpenCodeJson => &["$schema", "model", "small_model", "theme"],
        CursorCliConfig => &["model", "vimMode"],
        CopilotConfig => &["model", "theme", "banner", "denied_tools"],
        ClineSettings => &[
            "apiProvider",
            "planModeApiProvider",
            "actModeApiProvider",
            "planModeApiModelId",
            "actModeApiModelId",
        ],
        AntigravitySettings => &["model", "theme", "modelProvider", "exclude", "deny"],
        CodexToml => unreachable!(),
    };
    for k in keys {
        if let Some(v) = input.get(k) {
            out[*k] = v.clone();
        }
    }
    if *kind == ClaudeConfig {
        out["projects"] = json!({"/workspace":{"hasTrustDialogAccepted":true}});
    }
    if let Some(p) = input.get("permissions") {
        if !p.is_object() {
            return Err(EngineError::Config(
                "unknown permissions shape; preserve deny/ask rules manually".into(),
            ));
        }
        let mut kept = json!({});
        for k in ["deny", "ask"] {
            if let Some(v) = p.get(k) {
                if !v.is_array() || !v.as_array().unwrap().iter().all(Value::is_string) {
                    return Err(EngineError::Config("unknown deny/ask rule shape".into()));
                }
                kept[k] = v.clone();
            }
        }
        if let Some(mode) = p.get("defaultMode").and_then(Value::as_str) {
            if ["plan", "default"].contains(&mode) {
                kept["defaultMode"] = json!(mode);
            }
        }
        out["permissions"] = kept;
    }
    if *kind == OpenCodeJson {
        if let Some(p) = input.get("permission") {
            out["permission"] = restrictive(p)?;
        }
        if let Some(ps) = input.get("provider").and_then(Value::as_object) {
            for (id, p) in ps {
                if let Some(url) = p.pointer("/options/baseURL") {
                    if safe_endpoint(url) {
                        out["provider"][id] = json!({"options":{"baseURL":url}});
                    }
                }
            }
        }
    }
    if let Some(approval) = input.get("autoApprovalSettings") {
        if let Some(a) = approval.as_object() {
            let mut v = json!({});
            for (k, val) in a {
                if val.is_boolean() {
                    v[k] = json!(false);
                } else if k == "actions" && val.is_object() {
                    v[k] = val
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(|k| (k.clone(), json!(false)))
                        .collect::<serde_json::Map<_, _>>()
                        .into();
                } else {
                    return Err(EngineError::Config(
                        "unknown auto approval settings shape".into(),
                    ));
                }
            }
            out["autoApprovalSettings"] = v;
        } else {
            return Err(EngineError::Config("unknown auto approval settings".into()));
        }
    }
    preserve_restrictions(&input, &out)?;
    serde_json::to_vec_pretty(&out).map_err(|e| EngineError::Config(e.to_string()))
}
fn restrictive(v: &serde_json::Value) -> Result<serde_json::Value, EngineError> {
    use serde_json::{json, Value};
    match v {
        Value::String(s) if s == "deny" || s == "ask" => Ok(v.clone()),
        Value::String(s) if s == "allow" => Ok(json!({})),
        Value::Object(o) => {
            let mut m = serde_json::Map::new();
            for (k, v) in o {
                let kept = restrictive(v)?;
                if kept != json!({}) {
                    m.insert(k.clone(), kept);
                }
            }
            Ok(Value::Object(m))
        }
        _ => Err(EngineError::Config(
            "unknown permission rule; deny/ask rules cannot be discarded".into(),
        )),
    }
}

fn preserve_restrictions(
    input: &serde_json::Value,
    output: &serde_json::Value,
) -> Result<(), EngineError> {
    if input.as_str().is_some_and(|s| s == "deny" || s == "ask") && output != input {
        return Err(EngineError::Config("profile cannot preserve a restrictive deny/ask policy in an unsupported settings section".into()));
    }
    if let Some(object) = input.as_object() {
        for (key, value) in object {
            let lower = key.to_ascii_lowercase();
            if (lower.contains("deny")
                || lower.contains("denied")
                || lower == "ask"
                || lower.contains("exclusion")
                || lower == "exclude")
                && !value.is_null()
                && value != &serde_json::json!([])
                && value != &serde_json::json!({})
                && output.get(key) != Some(value)
            {
                return Err(EngineError::Config(format!("profile cannot preserve restrictive rule {key}; migrate it to the provider's supported deny/ask settings")));
            }
            preserve_restrictions(value, output.get(key).unwrap_or(&serde_json::Value::Null))?;
        }
    }
    Ok(())
}
