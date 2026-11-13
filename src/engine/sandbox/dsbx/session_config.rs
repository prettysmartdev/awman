//! Workspace-backed per-launch session config writer.
//!
//! Writes `<workspace>/.awman/session.json` — the only host→VM channel
//! available at launch time. The kit's `apply-session-config.sh` reads this
//! file on every sandbox start and renders the agent's in-VM config.
//!
//! **No credential values are ever written here.** Credentials go through
//! `sbx secret set` / proxy management (see [`super::auth`]); only
//! non-sensitive dynamic state lands in this workspace-readable file.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::engine::container::host_env_denylist::scrub_run_env;
use crate::engine::container::options::ModelFlagForm;
use crate::engine::error::EngineError;
use crate::engine::sandbox::dsbx::auth::is_credential_like;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// Schema version of the `session.json` contract. The writer and the bundled
/// `apply-session-config.sh` are versioned together; a mismatch makes the
/// startup script fail loudly and direct the user to re-run `awman ready`.
pub(super) const SESSION_SCHEMA_VERSION: u32 = 1;

/// Render only selected non-sensitive instance values, never the full snapshot.
pub(super) struct DSbxSessionConfig;

impl DSbxSessionConfig {
    /// The notice for host-terminal variables the options asked for that are
    /// not written to `session.json`, or `None`.
    pub(super) fn host_terminal_notice(options: &ResolvedSandboxOptions) -> Option<String> {
        let mut requested: Vec<(String, String)> = options
            .env_passthrough
            .iter()
            .map(|v| (v.0.clone(), String::new()))
            .collect();
        requested.extend(
            options
                .env_literal
                .iter()
                .map(|l| (l.key.clone(), String::new())),
        );
        scrub_run_env(requested, None).notice()
    }

    /// Build the `session.json` value for the given resolved options. Pure —
    /// no filesystem access — so it is trivially testable and so the
    /// credential-exclusion invariant can be asserted directly.
    pub(super) fn build_value(options: &ResolvedSandboxOptions) -> Value {
        let snapshot = options
            .host_env
            .clone()
            .unwrap_or_else(crate::engine::host_cli::process_env);
        let mut root = Map::new();
        root.insert("schema_version".into(), json!(SESSION_SCHEMA_VERSION));
        root.insert("agent".into(), json!(options.agent_id));
        root.insert("interactive".into(), json!(options.interactive));

        if let Some(ep) = &options.entrypoint_override {
            root.insert("entrypoint_override".into(), json!(ep.0));
        }
        if let Some(prompt) = &options.seeded_prompt {
            root.insert("seeded_prompt".into(), json!(prompt));
        }
        if let Some(model) = &options.model {
            let model_str = match model {
                ModelFlagForm::Argument(name) => name.clone(),
                ModelFlagForm::Shorthand(s) => s.clone(),
            };
            root.insert("model".into(), json!(model_str));
        }
        // Mixin permission mode ("plan" / "auto" / "yolo"): the apply script
        // renders the agent's native equivalent (claude: permissions.
        // defaultMode). `mode_flags` are argv-only (kind: agent kits) and
        // intentionally never land here.
        if let Some(mode) = &options.permission_mode {
            root.insert("permission_mode".into(), json!(mode));
        }

        // System prompts — workspace paths are identical inside the VM, so the
        // in-VM (container) path is what the startup script uses.
        if let Some((_host, container, flag)) = &options.system_prompt_file {
            root.insert(
                "system_prompt_file".into(),
                json!({ "path": container.display().to_string(), "flag": flag }),
            );
        }
        if let Some((env_var, _host, container)) = &options.system_prompt_env_file {
            root.insert(
                "system_prompt_env_file".into(),
                json!({ "env_var": env_var, "path": container.display().to_string() }),
            );
        }
        if let Some((flag, text)) = &options.system_prompt_inline {
            root.insert(
                "system_prompt_inline".into(),
                json!({ "flag": flag, "text": text }),
            );
        }

        root.insert("disallowed_tools".into(), json!(options.disallowed_tools));
        root.insert("allowed_tools".into(), json!(options.allowed_tools));

        // Non-sensitive env config only. Credential-class values are excluded
        // here and routed through `sbx secret set` / proxy management instead.
        //
        // Deliberately `std::env::var`, **not** `host_var`: this map is written
        // in the clear to `<workspace>/.awman/session.json`, and the only thing
        // keeping a value out of it is the `is_credential_like` *name*
        // heuristic. Reading through the daemon overlay would make a value a
        // squad client pushed over the socket eligible for a plaintext file the
        // moment its name did not happen to contain TOKEN/SECRET/PASSWORD. Squad
        // admits only container-tier runtimes today, so nothing reaches here
        // from a daemon — but that is an admission check in an unrelated module,
        // and this file must not depend on it. This process's own environment is
        // the only correct source for a writer like this one.
        let mut env_pairs: Vec<(String, String)> = Vec::new();
        for envvar in &options.env_passthrough {
            if is_credential_like(&envvar.0) {
                continue;
            }
            if let Some(value) = snapshot.get(&envvar.0) {
                env_pairs.push((envvar.0.clone(), value.to_owned()));
            }
        }
        for lit in &options.env_literal {
            if is_credential_like(&lit.key) {
                continue;
            }
            env_pairs.push((lit.key.clone(), lit.value.clone()));
        }
        // The one host-terminal scrub for this backend (WI 0141 §3).
        let mut env_config = Map::new();
        for (key, value) in scrub_run_env(env_pairs, snapshot.get("COLORTERM")).kept {
            env_config.insert(key, json!(value));
        }
        root.insert("env_config".into(), Value::Object(env_config));

        // Arbitrary structured agent settings (the Docker-side
        // AgentSettingsPassthrough analogue). These are not credentials.
        root.insert(
            "agent_settings".into(),
            Value::Object(
                options
                    .agent_settings
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );

        Value::Object(root)
    }

    /// Write `<workspace>/.awman/session.json` from the resolved options.
    /// Returns the path written.
    pub(super) fn write_for(
        options: &ResolvedSandboxOptions,
        workspace: &Path,
    ) -> Result<PathBuf, EngineError> {
        let dir = workspace.join(".awman");
        std::fs::create_dir_all(&dir).map_err(|e| EngineError::io(&dir, e))?;
        let path = dir.join("session.json");
        let value = Self::build_value(options);
        let content = serde_json::to_string_pretty(&value)
            .map_err(|e| EngineError::Sandbox(format!("serialize session.json: {e}")))?;
        std::fs::write(&path, content).map_err(|e| EngineError::io(&path, e))?;
        Ok(path)
    }
}
