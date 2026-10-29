//! Schema, validation and loading of custom agent definitions.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::data::config::{Env, GlobalConfig};
use crate::data::fs::agent_dirs::DefinitionScope;

pub const AGENT_DEFINITION_SCHEMA: u32 = 1;
/// Whole-file cap enforced before parsing.
pub const MAX_DEFINITION_BYTES: usize = 64 * 1024;
/// Every array in the file (argv, flags, install, apt, auth.env …) is capped here.
pub const MAX_ARRAY_LEN: usize = 64;
pub const MAX_ARGV_TOKEN_BYTES: usize = 1024;
pub const MAX_INSTALL_STEP_BYTES: usize = 4096;
pub const MAX_PING_TOKENS: usize = 16;

/// Reserved first-party names, shared by discovery and the built-in table.
pub const FIRST_PARTY_AGENT_NAMES: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "gemini",
    "copilot",
    "cline",
    "antigravity",
];

/// Host directories (relative to `$HOME`) a definition may never mount. Checked at parse
/// time here and again in `OverlayEngine` (defence in depth).
pub const HOST_DIR_DENY_LIST: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".awman",
    ".netrc",
    ".git-credentials",
    ".config/gh",
    ".password-store",
    ".local/share/keyrings",
    "Library/Keychains",
];

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinitionFile {
    pub schema: u32,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    pub run: RunTable,
    #[serde(default)]
    pub modes: ModesTable,
    #[serde(default)]
    pub tools: Option<ToolsTable>,
    #[serde(default)]
    pub system_prompt: Option<SystemPromptTable>,
    #[serde(default)]
    pub acp: Option<AcpTable>,
    #[serde(default)]
    pub ping: Option<PingTable>,
    #[serde(default)]
    pub settings: Option<SettingsTable>,
    #[serde(default)]
    pub auth: Option<AuthTable>,
    pub image: ImageTable,
    /// Reserved. Accepted by serde only so `validate()` can emit the dedicated
    /// "custom agents do not support Docker Sandboxes yet" message instead of the
    /// generic unknown-field error. Always a validation failure when `Some`.
    #[serde(default)]
    pub sandbox: Option<toml::Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunTable {
    pub command: Vec<String>,
    #[serde(default)]
    pub non_interactive: Option<String>,
    #[serde(default)]
    pub interactive_only_flags: Vec<String>,
    #[serde(default)]
    pub seed_prompt: SeedPromptSpec, // default Positional
    #[serde(default)]
    pub model: ModelFlagSpec, // default Space
    /// The structured output mode a non-interactive run can print in
    /// (WI 0137 §4): `"text"` (default), `"claude_stream_json"` or
    /// `"codex_json"`. A structured format lets an API run produce
    /// `step_activity` events and a `final_message`.
    #[serde(default)]
    pub output_format: crate::data::agent_output_format::AgentOutputFormat,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

/// `seed_prompt = "positional"` or `seed_prompt = { flag = "--prompt" }`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub enum SeedPromptSpec {
    Keyword(String),
    Flag { flag: String },
}
impl Default for SeedPromptSpec {
    fn default() -> Self {
        Self::Keyword("positional".into())
    }
}
// validate(): Keyword must be exactly "positional".

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModelFlagSpec {
    #[default]
    Space,
    Equals,
    Unsupported,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ModesTable {
    #[serde(default)]
    pub yolo: Vec<String>,
    #[serde(default)]
    pub auto: Vec<String>,
    #[serde(default)]
    pub plan: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolsTable {
    #[serde(default)]
    pub allowed_flag: Option<String>,
    #[serde(default)]
    pub disallowed_flag: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryKind {
    AppendFile,
    AppendInline,
    Replace,
    AgentsMd,
    EnvFile,
    AddDir,
    #[default]
    None,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SystemPromptTable {
    #[serde(default)]
    pub delivery: DeliveryKind,
    #[serde(default)]
    pub flag: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcpTable {
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PingTable {
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SettingsTable {
    #[serde(default)]
    pub host_dir: Option<String>,
    #[serde(default)]
    pub skills_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthTable {
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageTable {
    #[serde(default)]
    pub needs_node: bool,
    #[serde(default)]
    pub apt: Vec<String>,
    #[serde(default)]
    pub install: Vec<String>,
    #[serde(default)]
    pub binary: Option<String>,
    #[serde(default)]
    pub dockerfile: Option<String>,
}

pub fn is_first_party_name(name: &str) -> bool {
    FIRST_PARTY_AGENT_NAMES.contains(&name)
}

/// `^[a-z0-9][a-z0-9_-]{0,63}$`, hand-rolled. Err carries the human reason.
pub fn validate_agent_file_stem(stem: &str) -> Result<(), String> {
    let first_ok = stem
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !first_ok
        || stem.len() > 64
        || !stem
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err("agent file stem must match ^[a-z0-9][a-z0-9_-]{0,63}$".into());
    }
    Ok(())
}

/// Whether a relative path overlaps a protected directory, including ancestors.
/// Fold ASCII case on every host so definitions stay safe across operating systems.
pub fn is_deny_listed_host_dir(rel: &str) -> bool {
    let folded = rel.to_ascii_lowercase();
    let path = Path::new(&folded);
    HOST_DIR_DENY_LIST.iter().any(|denied| {
        let denied = denied.to_ascii_lowercase();
        path.starts_with(&denied) || Path::new(&denied).starts_with(path)
    })
}

/// Unicode bidirectional formatting and invisible joiner characters: they
/// can reorder or hide text in a terminal or TUI, so a reviewed argv would
/// not read as what runs.
pub fn is_bidi_or_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// Control (C0, DEL, C1) or bidi/invisible formatting characters: rejected in
/// every definition string.
pub fn has_unsafe_display_chars(value: &str) -> bool {
    value
        .chars()
        .any(|c| c.is_control() || is_bidi_or_invisible_format(c))
}

/// `s` with every control or bidi/invisible formatting character shown as a
/// visible escape (`\u{1b}`), except line breaks. For untrusted text at a
/// presentation boundary: diagnostics quote keys and paths from a definition
/// that may have been rejected precisely for containing them.
pub fn escape_for_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c != '\n' && (c.is_control() || is_bidi_or_invisible_format(c)) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// POSIX env name: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn is_posix_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn check_token(field: &str, value: &str, problems: &mut Vec<String>) {
    if value.is_empty() {
        problems.push(format!("{field}: argv token must not be empty"));
    }
    if has_unsafe_display_chars(value) {
        problems.push(format!(
            "{field}: argv token must not contain control characters"
        ));
    }
    if value.len() > MAX_ARGV_TOKEN_BYTES {
        problems.push(format!(
            "{field}: argv token exceeds {MAX_ARGV_TOKEN_BYTES} bytes"
        ));
    }
}

fn check_array_len(field: &str, len: usize, problems: &mut Vec<String>) {
    if len > MAX_ARRAY_LEN {
        problems.push(format!("{field}: array exceeds {MAX_ARRAY_LEN} entries"));
    }
}

fn check_argv(field: &str, argv: &[String], command: bool, problems: &mut Vec<String>) {
    check_array_len(field, argv.len(), problems);
    if command && argv.is_empty() {
        problems.push(format!("{field}: command must not be empty"));
    }
    for (index, token) in argv.iter().enumerate() {
        check_token(&format!("{field}[{index}]"), token, problems);
    }
    if command {
        if let Some(binary) = argv.first() {
            if field == "ping.command" {
                check_host_binary(&format!("{field}[0]"), binary, problems);
            } else {
                check_binary(&format!("{field}[0]"), binary, problems);
            }
        }
    }
}

/// Whether `binary` is one plain file name for a host `PATH` lookup: no
/// separator of either kind, and no drive or root prefix (`C:pi.exe` is
/// drive-relative on Windows, not bare).
pub fn is_bare_host_binary(binary: &str) -> bool {
    !binary.contains(['/', '\\'])
        && matches!(
            Path::new(binary).components().collect::<Vec<_>>()[..],
            [std::path::Component::Normal(_)]
        )
}

/// The host side of `ping.command[0]`: a fully absolute host path or a bare
/// name. Rooted-but-relative (`\\tools\\pi.exe` on Windows) and
/// drive-relative (`C:pi.exe`) forms would resolve against an implicit
/// current drive or directory, so they are refused.
fn check_host_binary(field: &str, binary: &str, problems: &mut Vec<String>) {
    if !Path::new(binary).is_absolute() && !is_bare_host_binary(binary) {
        problems.push(format!(
            "{field}: must be a bare binary name or an absolute host path; relative, rooted and drive-relative paths are not allowed"
        ));
    }
}

fn check_binary(field: &str, binary: &str, problems: &mut Vec<String>) {
    // Container paths use '/' on every host. Relative paths must never resolve
    // against repository content, including when used by the optional host ping.
    if !binary.starts_with('/') && (binary.contains(['/', '\\']) || binary == "." || binary == "..")
    {
        problems.push(format!("{field}: must be a bare binary name or an absolute path; relative paths are not allowed"));
    }
}

fn normal_relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\0', '\\'])
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

impl AgentDefinitionFile {
    /// Return all semantic violations in field order. Filesystem checks live in the loader.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (field, value, cap) in [
            ("display_name", Some(&self.display_name), 256),
            ("description", self.description.as_ref(), 4096),
            ("homepage", self.homepage.as_ref(), 2048),
        ] {
            if let Some(value) = value {
                if has_unsafe_display_chars(value) || value.len() > cap {
                    problems.push(format!(
                        "{field}: must contain no control characters and be at most {cap} bytes"
                    ));
                }
            }
        }
        if self.schema > AGENT_DEFINITION_SCHEMA {
            problems.push(format!(
                "schema {} requires a newer awman (this build supports 1)",
                self.schema
            ));
        } else if self.schema != AGENT_DEFINITION_SCHEMA {
            problems.push("schema: must be 1".into());
        }
        check_argv("run.command", &self.run.command, true, &mut problems);
        if let Some(token) = &self.run.non_interactive {
            check_token("run.non_interactive", token, &mut problems);
        }
        check_argv(
            "run.interactive_only_flags",
            &self.run.interactive_only_flags,
            false,
            &mut problems,
        );
        match &self.run.seed_prompt {
            SeedPromptSpec::Keyword(keyword) if keyword != "positional" => problems
                .push("run.seed_prompt: must be \"positional\" or { flag = \"--prompt\" }".into()),
            SeedPromptSpec::Flag { flag } => {
                check_token("run.seed_prompt.flag", flag, &mut problems)
            }
            _ => {}
        }
        for (name, value) in &self.run.env {
            if has_unsafe_display_chars(value) {
                problems.push(format!(
                    "run.env.{name}: must not contain control characters"
                ));
            }
            if !is_posix_env_name(name) {
                problems.push(format!(
                    "run.env.{name}: must be a POSIX environment variable name"
                ));
            }
            if matches!(name.as_str(), "HOME" | "PATH" | "USER")
                || name.starts_with("AWMAN_")
                || name.starts_with("LD_")
            {
                problems.push(format!("run.env.{name}: reserved environment variable"));
            }
        }
        check_argv("modes.yolo", &self.modes.yolo, false, &mut problems);
        check_argv("modes.auto", &self.modes.auto, false, &mut problems);
        check_argv("modes.plan", &self.modes.plan, false, &mut problems);
        if let Some(tools) = &self.tools {
            for (field, token) in [
                ("tools.allowed_flag", &tools.allowed_flag),
                ("tools.disallowed_flag", &tools.disallowed_flag),
            ] {
                if let Some(token) = token {
                    check_token(field, token, &mut problems);
                }
            }
        }
        if let Some(prompt) = &self.system_prompt {
            let flag_required = matches!(
                prompt.delivery,
                DeliveryKind::AppendFile
                    | DeliveryKind::AppendInline
                    | DeliveryKind::Replace
                    | DeliveryKind::AddDir
            );
            for (field, value, required) in [
                ("system_prompt.flag", &prompt.flag, flag_required),
                (
                    "system_prompt.key",
                    &prompt.key,
                    prompt.delivery == DeliveryKind::AppendInline,
                ),
                (
                    "system_prompt.env",
                    &prompt.env,
                    prompt.delivery == DeliveryKind::EnvFile,
                ),
            ] {
                match (value, required) {
                    (None, true) => problems.push(format!(
                        "{field}: required for delivery {:?}",
                        prompt.delivery
                    )),
                    (Some(_), false) => problems.push(format!(
                        "{field}: not allowed for delivery {:?}",
                        prompt.delivery
                    )),
                    _ => {}
                }
                if let Some(value) = value {
                    check_token(field, value, &mut problems);
                }
            }
            if let Some(env) = &prompt.env {
                if !is_posix_env_name(env) {
                    problems.push(
                        "system_prompt.env: must be a POSIX environment variable name".into(),
                    );
                }
            }
        }
        if let Some(acp) = &self.acp {
            check_argv("acp.command", &acp.command, true, &mut problems);
        }
        if let Some(ping) = &self.ping {
            check_argv("ping.command", &ping.command, true, &mut problems);
            if ping.command.len() > MAX_PING_TOKENS {
                problems.push(format!(
                    "ping.command: must have at most {MAX_PING_TOKENS} tokens"
                ));
            }
        }
        if let Some(settings) = &self.settings {
            if let Some(host_dir) = &settings.host_dir {
                check_token("settings.host_dir", host_dir, &mut problems);
                if !normal_relative(host_dir) {
                    problems.push("settings.host_dir: must be a non-empty, normalised relative path (no '.', '..' or leading '/')".into());
                }
                if is_deny_listed_host_dir(host_dir) {
                    problems.push(format!("settings.host_dir: '{host_dir}' is on or under the host directory deny-list"));
                }
            }
            if let Some(skills_dir) = &settings.skills_dir {
                check_token("settings.skills_dir", skills_dir, &mut problems);
                if !normal_relative(skills_dir) {
                    problems.push(
                        "settings.skills_dir: must be a non-empty, normalised relative path".into(),
                    );
                }
            }
        }
        if let Some(auth) = &self.auth {
            check_array_len("auth.env", auth.env.len(), &mut problems);
            for (index, name) in auth.env.iter().enumerate() {
                if !is_posix_env_name(name) {
                    problems.push(format!(
                        "auth.env[{index}]: must be a POSIX environment variable name"
                    ));
                }
                if name.starts_with("AWMAN_") || name == "SSH_AUTH_SOCK" {
                    problems.push(format!(
                        "auth.env[{index}]: '{name}' is a reserved environment variable"
                    ));
                }
            }
        }
        check_array_len("image.apt", self.image.apt.len(), &mut problems);
        for (index, package) in self.image.apt.iter().enumerate() {
            let first_ok = package
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
            if !first_ok
                || !package.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'.' | b'-')
                })
            {
                problems.push(format!(
                    "image.apt[{index}]: package must match ^[a-z0-9][a-z0-9+.-]*$"
                ));
            }
        }
        check_array_len("image.install", self.image.install.len(), &mut problems);
        for (index, step) in self.image.install.iter().enumerate() {
            if step.trim().is_empty() {
                problems.push(format!(
                    "image.install[{index}]: install step must not be empty"
                ));
            }
            if has_unsafe_display_chars(step) {
                problems.push(format!(
                    "image.install[{index}]: install step must be single-line with no control characters"
                ));
            }
            if step.len() > MAX_INSTALL_STEP_BYTES {
                problems.push(format!(
                    "image.install[{index}]: install step exceeds {MAX_INSTALL_STEP_BYTES} bytes"
                ));
            }
        }
        if let Some(binary) = &self.image.binary {
            check_token("image.binary", binary, &mut problems);
            check_binary("image.binary", binary, &mut problems);
        }
        if let Some(dockerfile) = &self.image.dockerfile {
            check_token("image.dockerfile", dockerfile, &mut problems);
            if !normal_relative(dockerfile) {
                problems.push("image.dockerfile: must be a relative path inside the TOML directory with only normal components".into());
            }
            if self.image.needs_node || !self.image.apt.is_empty() || !self.image.install.is_empty()
            {
                problems.push(
                    "image.dockerfile: mutually exclusive with install, apt and needs_node".into(),
                );
            }
        } else if self.image.install.is_empty() {
            problems.push("image.install: required unless image.dockerfile is set".into());
        }
        if self.sandbox.is_some() {
            problems.push("custom agents do not support Docker Sandboxes yet; remove [sandbox] and use docker, apple or builtin-experimental".into());
        }
        problems
    }

    pub fn smoke_binary(&self) -> String {
        self.image.binary.clone().unwrap_or_else(|| {
            self.run
                .command
                .first()
                .and_then(|command| Path::new(command).file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", render_problems(.path, .problems))]
pub struct AgentDefinitionInvalid {
    pub path: PathBuf,
    pub problems: Vec<String>,
}

fn render_problems(path: &Path, problems: &[String]) -> String {
    let path = escape_for_display(&path.display().to_string());
    problems
        .iter()
        .map(|problem| format!("{path}: {}", escape_for_display(problem)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone)]
pub struct LoadedAgentDefinition {
    pub file: AgentDefinitionFile,
    pub path: PathBuf,
    pub toml_bytes: Vec<u8>,
    pub dockerfile: Option<(PathBuf, Vec<u8>)>,
    pub sha256: String,
}

#[derive(Deserialize)]
struct SchemaProbe {
    schema: Option<u64>,
}

pub fn content_hash(toml_bytes: &[u8], dockerfile_bytes: Option<&[u8]>) -> String {
    let mut hash = Sha256::new();
    hash.update(toml_bytes);
    if let Some(bytes) = dockerfile_bytes {
        hash.update(b"\n--awman-dockerfile--\n");
        hash.update(bytes);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Load using the process's global data home to identify the scope for diagnostics.
/// Callers with an explicit environment/scope should use `load_agent_definition_with_scope`.
pub fn load_agent_definition(path: &Path) -> Result<LoadedAgentDefinition, AgentDefinitionInvalid> {
    let global = GlobalConfig::data_home_with(&Env::from_process())
        .ok()
        .map(|home| home.join("agents"));
    let parent = path.parent().unwrap_or(Path::new("."));
    let scope = if global.is_some_and(|dir| {
        dir == parent
            || std::fs::canonicalize(&dir)
                .ok()
                .zip(std::fs::canonicalize(parent).ok())
                .is_some_and(|(a, b)| a == b)
    }) {
        DefinitionScope::Global
    } else {
        DefinitionScope::Repo
    };
    load_agent_definition_with_scope(path, scope)
}

/// Explicit scope avoids guessing when the caller uses an injected environment snapshot.
pub fn load_agent_definition_with_scope(
    path: &Path,
    scope: DefinitionScope,
) -> Result<LoadedAgentDefinition, AgentDefinitionInvalid> {
    let fail = |problem: String| AgentDefinitionInvalid {
        path: path.to_path_buf(),
        problems: vec![format!("{problem} [scope: {}]", scope.as_str())],
    };
    let meta = std::fs::metadata(path).map_err(|e| fail(format!("cannot read definition: {e}")))?;
    if !meta.is_file() {
        return Err(fail("not a regular file".into()));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|e| fail(format!("cannot read definition: {e}")))?;
    if !file.metadata().map_err(|e| fail(e.to_string()))?.is_file() {
        return Err(fail("not a regular file".into()));
    }
    let mut toml_bytes = Vec::new();
    file.take((MAX_DEFINITION_BYTES + 1) as u64)
        .read_to_end(&mut toml_bytes)
        .map_err(|e| fail(format!("cannot read definition: {e}")))?;
    if toml_bytes.len() > MAX_DEFINITION_BYTES {
        return Err(fail(format!(
            "definition exceeds {MAX_DEFINITION_BYTES} bytes (64 KiB)"
        )));
    }
    let source = std::str::from_utf8(&toml_bytes)
        .map_err(|e| fail(format!("definition is not UTF-8: {e}")))?;
    let parse_error = |error: toml::de::Error| {
        let location = error
            .span()
            .map(|span| source_location(source, span))
            .unwrap_or_else(|| "line 1, column 1".into());
        fail(format!("{error} [{location}]"))
    };
    let probe: SchemaProbe = toml::from_str(source).map_err(parse_error)?;
    if let Some(schema) = probe
        .schema
        .filter(|schema| *schema > u64::from(AGENT_DEFINITION_SCHEMA))
    {
        return Err(fail(format!(
            "schema {schema} requires a newer awman (this build supports 1) [{}]",
            field_location(source, "schema")
        )));
    }
    let file: AgentDefinitionFile = toml::from_str(source).map_err(parse_error)?;
    let mut problems = file.validate();
    // These fields have default values in the public schema. Inspect presence as
    // well: an explicit `needs_node = false` or `apt = []` is still excluded.
    let raw: toml::Table = toml::from_str(source).map_err(parse_error)?;
    if file.image.dockerfile.is_some() {
        if let Some(image) = raw.get("image").and_then(toml::Value::as_table) {
            if ["needs_node", "apt", "install"]
                .iter()
                .any(|key| image.contains_key(*key))
                && !problems
                    .iter()
                    .any(|p| p.starts_with("image.dockerfile: mutually exclusive"))
            {
                problems.push(
                    "image.dockerfile: mutually exclusive with install, apt and needs_node".into(),
                );
            }
        }
    }
    let dockerfile = file
        .image
        .dockerfile
        .as_ref()
        .filter(|rel| normal_relative(rel))
        .and_then(|rel| match read_dockerfile(path, rel) {
            Ok(value) => Some(value),
            Err(problem) => {
                problems.push(problem);
                None
            }
        });
    if !problems.is_empty() {
        let problems = problems
            .into_iter()
            .map(|problem| {
                let field = if problem.starts_with("custom agents do not support") {
                    "sandbox"
                } else if problem.starts_with("schema ") {
                    "schema"
                } else {
                    problem.split(':').next().unwrap_or("")
                };
                format!(
                    "{problem} [scope: {}; {}]",
                    scope.as_str(),
                    field_location(source, field)
                )
            })
            .collect();
        return Err(AgentDefinitionInvalid {
            path: path.to_path_buf(),
            problems,
        });
    }
    let sha256 = content_hash(
        &toml_bytes,
        dockerfile.as_ref().map(|(_, bytes)| bytes.as_slice()),
    );
    Ok(LoadedAgentDefinition {
        file,
        path: path.to_path_buf(),
        toml_bytes,
        dockerfile,
        sha256,
    })
}

fn read_dockerfile(definition: &Path, relative: &str) -> Result<(PathBuf, Vec<u8>), String> {
    let dir = definition
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let path = dir.join(relative);
    let fail = |reason: String| format!("image.dockerfile: {}: {reason}", path.display());
    crate::data::fs::path_guard::ensure_not_symlinked(&path).map_err(|e| fail(e.to_string()))?;
    let meta = std::fs::symlink_metadata(&path).map_err(|e| fail(e.to_string()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(fail("must be a regular file, not a symlink".into()));
    }
    let canonical_dir = std::fs::canonicalize(dir).map_err(|e| fail(e.to_string()))?;
    let canonical = std::fs::canonicalize(&path).map_err(|e| fail(e.to_string()))?;
    if !canonical.starts_with(&canonical_dir) {
        return Err(fail(format!("resolves outside {}", dir.display())));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&canonical)
        .map_err(|e| fail(e.to_string()))?
        .take((MAX_DEFINITION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(e.to_string()))?;
    if bytes.len() > MAX_DEFINITION_BYTES {
        return Err(fail(format!(
            "exceeds {MAX_DEFINITION_BYTES} bytes (64 KiB)"
        )));
    }
    Ok((canonical, bytes))
}

fn source_location(source: &str, span: std::ops::Range<usize>) -> String {
    let position = |offset: usize| {
        let prefix = &source[..offset.min(source.len())];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
        let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        (line, column)
    };
    let (line, column) = position(span.start);
    let (end_line, end_column) = position(span.end);
    format!("line {line}, column {column} to line {end_line}, column {end_column}")
}

// Use the TOML parser's own spanned syntax tree for semantic diagnostics.
fn field_location(source: &str, field: &str) -> String {
    let Ok(root) = toml::de::DeTable::parse(source) else {
        return "line 1, column 1".into();
    };
    let mut table = root.get_ref();
    let mut span = root.span();
    for segment in field.split('.') {
        let (key, index) = segment
            .split_once('[')
            .map_or((segment, None), |(key, rest)| {
                (
                    key,
                    rest.strip_suffix(']').and_then(|i| i.parse::<usize>().ok()),
                )
            });
        let Some(child) = table.get(key) else {
            break;
        };
        let mut node = child;
        if let Some(index) = index {
            if let toml::de::DeValue::Array(values) = node.get_ref() {
                if let Some(child) = values.get(index) {
                    node = child;
                }
            }
        }
        span = node.span();
        let Some(child_table) = node.get_ref().as_table() else {
            break;
        };
        table = child_table;
    }
    source_location(source, span)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> AgentDefinitionFile {
        toml::from_str(
            "schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[image]\ninstall=['install pi']\n",
        )
        .unwrap()
    }

    #[test]
    fn protected_paths_include_ancestors_case_and_descendants() {
        for path in [
            ".config",
            ".local",
            "Library",
            ".SSH",
            ".Aws/config",
            "library/keychains",
            ".config/GH/tokens",
        ] {
            assert!(is_deny_listed_host_dir(path), "{path}");
            let mut file = minimal();
            file.settings = Some(SettingsTable {
                host_dir: Some(path.into()),
                skills_dir: None,
            });
            assert!(!file.validate().is_empty(), "{path}");
        }
        for path in [".config/crush", ".ssh-agent", "Library/Preferences"] {
            assert!(!is_deny_listed_host_dir(path), "{path}");
        }
    }

    #[test]
    fn displayed_fields_reject_all_terminal_controls_and_excessive_lengths() {
        for control in [
            '\0', '\t', '\n', '\r', '\u{1b}', '\u{7f}', '\u{85}', '\u{9b}',
        ] {
            let value = format!("Pi{control}hidden");
            let mut file = minimal();
            file.display_name = value.clone();
            file.description = Some(value.clone());
            file.homepage = Some(value.clone());
            file.run.command.push(value.clone());
            file.run.env.insert("PI_TEST".into(), value.clone());
            file.image.install = vec![value.clone()];
            file.settings = Some(SettingsTable {
                host_dir: Some(value.clone()),
                skills_dir: Some(value.clone()),
            });
            file.system_prompt = Some(SystemPromptTable {
                delivery: DeliveryKind::AppendInline,
                flag: Some(value.clone()),
                key: Some(value),
                env: None,
            });
            let problems = file.validate();
            for field in [
                "display_name",
                "description",
                "homepage",
                "run.command",
                "run.env",
                "image.install",
                "settings.host_dir",
                "settings.skills_dir",
                "system_prompt.flag",
                "system_prompt.key",
            ] {
                assert!(
                    problems
                        .iter()
                        .any(|p| p.starts_with(field) && p.contains("control")),
                    "{control:?} {field}: {problems:?}"
                );
            }
        }
        let mut file = minimal();
        file.display_name = "x".repeat(257);
        file.description = Some("x".repeat(4097));
        file.homepage = Some("x".repeat(2049));
        assert_eq!(file.validate().len(), 3);
    }

    #[test]
    fn skills_destination_must_be_a_normal_relative_directory() {
        for path in ["", ".", "./skills", "a/../b", "/skills"] {
            let mut file = minimal();
            file.settings = Some(SettingsTable {
                host_dir: None,
                skills_dir: Some(path.into()),
            });
            assert!(!file.validate().is_empty(), "{path}");
        }
    }

    #[test]
    fn escape_hatch_has_a_bounded_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pi.toml");
        std::fs::write(&path, "schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[image]\ndockerfile='pi.Dockerfile'\n").unwrap();
        std::fs::write(
            dir.path().join("pi.Dockerfile"),
            vec![b'x'; MAX_DEFINITION_BYTES + 1],
        )
        .unwrap();
        let error = load_agent_definition_with_scope(&path, DefinitionScope::Repo).unwrap_err();
        assert!(error.to_string().contains("image.dockerfile"));
        assert!(error.to_string().contains("64 KiB"));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_definitions_are_rejected_without_opening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pi.toml");
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let error = load_agent_definition_with_scope(&path, DefinitionScope::Repo).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn load_minimal_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pi.toml");
        let source = "schema = 1\ndisplay_name = 'Pi'\n[run]\ncommand = ['pi']\n[image]\ninstall = ['install pi']\n";
        std::fs::write(&path, source).unwrap();
        let loaded = load_agent_definition_with_scope(&path, DefinitionScope::Repo).unwrap();
        assert!(loaded.file.validate().is_empty());
        assert_eq!(loaded.file.smoke_binary(), "pi");
        assert_eq!(loaded.sha256, content_hash(source.as_bytes(), None));
        assert!(field_location(source, "run.command[0]").starts_with("line 4, column 12"));
    }

    #[test]
    fn validation_table_accepts_valid_values_and_accumulates_all_invalid_rows() {
        for stem in ["a", "0", "agent_1", "agent-x", &"a".repeat(64)] {
            assert!(validate_agent_file_stem(stem).is_ok(), "{stem:?}");
        }
        for stem in ["", "_agent", "-agent", "A", "a/b", &"a".repeat(65)] {
            assert!(validate_agent_file_stem(stem).is_err(), "{stem:?}");
        }
        assert!(is_first_party_name("claude"));
        assert!(!is_first_party_name("pi"));

        let valid = "schema=1\ndisplay_name='Pi'\nhomepage='https://example.test'\n[run]\ncommand=['/usr/bin/pi']\nnon_interactive='run'\ninteractive_only_flags=['--interactive']\nseed_prompt={flag='--prompt'}\nmodel='equals'\nenv={PI_TEST='1'}\n[modes]\nyolo=['--yolo']\nauto=['--auto']\nplan=['--plan']\n[tools]\nallowed_flag='--allow'\ndisallowed_flag='--deny'\n[system_prompt]\ndelivery='append-inline'\nflag='--config'\nkey='instructions'\n[acp]\ncommand=['pi','--acp']\n[ping]\ncommand=['/usr/bin/pi','--ping']\n[settings]\nhost_dir='.config/pi'\nskills_dir='skills/pi'\n[auth]\nenv=['PI_TOKEN']\n[image]\napt=['ripgrep','libssl3']\ninstall=['install pi']\nbinary='pi'\n";
        let accepted: AgentDefinitionFile = toml::from_str(valid).unwrap();
        assert!(accepted.validate().is_empty(), "{:?}", accepted.validate());

        // One deliberately invalid file exercises every semantic validation row. The test
        // checks the complete set of independent diagnostics, not merely the first error.
        let invalid = "schema=2\ndisplay_name=\"bad\\nname\"\ndescription='ok'\n[run]\ncommand=['./pi','']\nnon_interactive=''\ninteractive_only_flags=[\"x\\ny\"]\nseed_prompt='not-positional'\nmodel='space'\nenv={'BAD-NAME'='x', HOME='x', AWMAN_BAD='x', LD_BAD='x', OK=\"x\\ny\"}\n[modes]\nyolo=['']\nauto=[\"x\\ny\"]\nplan=['x']\n[tools]\nallowed_flag=''\ndisallowed_flag='--deny'\n[system_prompt]\ndelivery='append-inline'\nflag='--config'\nenv='NOT_ALLOWED'\n[acp]\ncommand=[]\n[ping]\ncommand=['bin/pi']\n[settings]\nhost_dir='../.ssh'\nskills_dir='./skills'\n[auth]\nenv=['AWMAN_X','SSH_AUTH_SOCK','bad-name']\n[image]\napt=['Bad_pkg']\ninstall=['  ',\"echo\\nhi\"]\nbinary='./pi'\n";
        let rejected: AgentDefinitionFile = toml::from_str(invalid).unwrap();
        let problems = rejected.validate();
        for field in [
            "display_name",
            "schema 2 requires a newer awman",
            "run.command[0]",
            "run.command[1]",
            "run.non_interactive",
            "run.interactive_only_flags[0]",
            "run.seed_prompt",
            "run.env.BAD-NAME",
            "run.env.HOME",
            "run.env.AWMAN_BAD",
            "run.env.LD_BAD",
            "run.env.OK",
            "modes.yolo[0]",
            "modes.auto[0]",
            "tools.allowed_flag",
            "system_prompt.key",
            "system_prompt.env",
            "acp.command",
            "ping.command[0]",
            "settings.host_dir",
            "settings.skills_dir",
            "auth.env[0]",
            "auth.env[1]",
            "auth.env[2]",
            "image.apt[0]",
            "image.install[0]",
            "image.install[1]",
            "image.binary",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(field)),
                "missing {field}: {problems:#?}"
            );
        }
        assert!(
            problems.len() >= 27,
            "expected accumulated violations: {problems:#?}"
        );
    }

    #[test]
    fn unknown_field_has_toml_location_and_sandbox_has_dedicated_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pi.toml");
        std::fs::write(&path, "schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[image]\ninstall=['x']\n[settings]\nhostdir='typo'\n").unwrap();
        let error = load_agent_definition_with_scope(&path, DefinitionScope::Repo)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `hostdir`"), "{error}");
        assert!(error.contains("line 8, column 1"), "{error}");

        std::fs::write(
            &path,
            "schema=2\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[image]\ninstall=['x']\n",
        )
        .unwrap();
        let newer = load_agent_definition_with_scope(&path, DefinitionScope::Repo)
            .unwrap_err()
            .to_string();
        assert!(
            newer.contains("schema 2 requires a newer awman (this build supports 1)"),
            "{newer}"
        );

        let sandbox = "schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[image]\ninstall=['x']\n[sandbox]\nfoo='bar'\n";
        let file: AgentDefinitionFile = toml::from_str(sandbox).unwrap();
        assert!(file
            .validate()
            .iter()
            .any(|p| p.contains("custom agents do not support Docker Sandboxes yet")));
    }

    #[test]
    fn content_hash_tracks_escape_hatch_bytes_but_not_mtime() {
        let toml = b"schema=1\ndisplay_name='Pi'";
        let first = content_hash(toml, Some(b"FROM base\n"));
        assert_eq!(first, content_hash(toml, Some(b"FROM base\n")));
        assert_ne!(first, content_hash(toml, Some(b"FROM changed\n")));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pi.toml");
        let source = "schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[image]\ndockerfile='pi.Dockerfile'\n";
        std::fs::write(&path, source).unwrap();
        std::fs::write(
            dir.path().join("pi.Dockerfile"),
            "FROM {{AWMAN_BASE_IMAGE}}\n",
        )
        .unwrap();
        let a = load_agent_definition_with_scope(&path, DefinitionScope::Repo)
            .unwrap()
            .sha256;
        let touched = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(touched))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(dir.path().join("pi.Dockerfile"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(touched))
            .unwrap();
        let b = load_agent_definition_with_scope(&path, DefinitionScope::Repo)
            .unwrap()
            .sha256;
        assert_eq!(a, b);
    }

    #[test]
    fn host_ping_binary_is_absolute_on_this_host_or_one_bare_name() {
        for bare in ["pi", "pi.exe", "pi-cli"] {
            assert!(is_bare_host_binary(bare), "{bare}");
        }
        for not_bare in ["./pi", "bin/pi", r"bin\pi", ".", "..", "", "/pi", r"\pi"] {
            assert!(!is_bare_host_binary(not_bare), "{not_bare:?}");
        }
        let problems = |binary: &str| {
            let mut problems = Vec::new();
            check_host_binary("ping.command[0]", binary, &mut problems);
            problems
        };
        #[cfg(windows)]
        {
            for ok in [r"C:\tools\pi.exe", r"\\server\share\pi.exe", "pi.exe"] {
                assert!(problems(ok).is_empty(), "{ok}");
            }
            for refused in [
                "C:pi.exe",
                r"\tools\pi.exe",
                "/tools/pi.exe",
                r"tools\pi.exe",
            ] {
                assert!(!problems(refused).is_empty(), "{refused}");
                assert!(!is_bare_host_binary(refused), "{refused}");
            }
        }
        #[cfg(unix)]
        {
            for ok in ["/usr/bin/pi", "pi"] {
                assert!(problems(ok).is_empty(), "{ok}");
            }
            for refused in ["./pi", "bin/pi", r"bin\pi"] {
                assert!(!problems(refused).is_empty(), "{refused}");
            }
        }
    }
    #[test]
    fn definition_size_and_ping_command_constraints_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pi.toml");
        std::fs::write(&path, vec![b' '; MAX_DEFINITION_BYTES + 1]).unwrap();
        let error = load_agent_definition_with_scope(&path, DefinitionScope::Repo).unwrap_err();
        assert!(error.to_string().contains("64 KiB"));

        let absolute = if cfg!(windows) {
            r"['C:\tools\pi.exe']"
        } else {
            "['/usr/bin/pi']"
        };
        for command in ["['pi']", absolute] {
            let source = format!("schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[ping]\ncommand={command}\n[image]\ninstall=['x']\n");
            let file: AgentDefinitionFile = toml::from_str(&source).unwrap();
            assert!(
                file.validate().is_empty(),
                "{command}: {:?}",
                file.validate()
            );
        }
        for (command, field) in [
            ("[]", "ping.command: command must not be empty"),
            ("['./pi']", "ping.command[0]"),
            ("['bin/pi']", "ping.command[0]"),
        ] {
            let source = format!("schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[ping]\ncommand={command}\n[image]\ninstall=['x']\n");
            let file: AgentDefinitionFile = toml::from_str(&source).unwrap();
            assert!(
                file.validate()
                    .iter()
                    .any(|problem| problem.contains(field)),
                "{command}: {:?}",
                file.validate()
            );
        }
        let long = std::iter::repeat_n("'x'", MAX_PING_TOKENS + 1)
            .collect::<Vec<_>>()
            .join(",");
        let source = format!("schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[ping]\ncommand=['pi',{long}]\n[image]\ninstall=['x']\n");
        let file: AgentDefinitionFile = toml::from_str(&source).unwrap();
        assert!(file
            .validate()
            .iter()
            .any(|problem| problem.contains("ping.command: must have at most 16 tokens")));
        for token in ["bad\\u0000token", "bad\\ntoken"] {
            let source = format!("schema=1\ndisplay_name='Pi'\n[run]\ncommand=['pi']\n[ping]\ncommand=['pi',\"{token}\"]\n[image]\ninstall=['x']\n");
            let file: AgentDefinitionFile = toml::from_str(&source).unwrap();
            assert!(
                file.validate()
                    .iter()
                    .any(|problem| problem.contains("ping.command[1]")
                        && problem.contains("control")),
                "{token}: {:?}",
                file.validate()
            );
        }
    }

    #[test]
    fn argv_array_install_and_dockerfile_validation_rows_have_boundaries() {
        let mut file = minimal();
        file.run.command = vec!["pi".into(), "x".repeat(MAX_ARGV_TOKEN_BYTES + 1)];
        file.run.interactive_only_flags = vec!["--x".into(); MAX_ARRAY_LEN + 1];
        file.image.apt = vec!["valid-package".into(); MAX_ARRAY_LEN + 1];
        file.image.install = vec!["x".repeat(MAX_INSTALL_STEP_BYTES + 1)];
        let problems = file.validate();
        for expected in [
            "run.command[1]: argv token exceeds",
            "run.interactive_only_flags: array exceeds",
            "image.apt: array exceeds",
            "image.install[0]: install step exceeds",
        ] {
            assert!(
                problems.iter().any(|problem| problem.contains(expected)),
                "missing {expected}: {problems:#?}"
            );
        }
        let mut dockerfile = minimal();
        dockerfile.image.install.clear();
        dockerfile.image.dockerfile = Some("../Dockerfile".into());
        let problems = dockerfile.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("image.dockerfile: must be a relative path")),
            "{problems:#?}"
        );
        dockerfile.image.dockerfile = Some("pi.Dockerfile".into());
        dockerfile.image.needs_node = true;
        assert!(dockerfile
            .validate()
            .iter()
            .any(|p| p.contains("mutually exclusive")));
    }
}
