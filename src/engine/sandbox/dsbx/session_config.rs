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

use crate::engine::container::options::ModelFlagForm;
use crate::engine::error::EngineError;
use crate::engine::sandbox::dsbx::auth::is_credential_like;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// Schema version of the `session.json` contract. The writer and the bundled
/// `apply-session-config.sh` are versioned together; a mismatch makes the
/// startup script fail loudly and direct the user to re-run `awman ready`.
pub(super) const SESSION_SCHEMA_VERSION: u32 = 1;

pub(super) struct DSbxSessionConfig;

impl DSbxSessionConfig {
    /// Build the `session.json` value for the given resolved options. Pure —
    /// no filesystem access — so it is trivially testable and so the
    /// credential-exclusion invariant can be asserted directly.
    pub(super) fn build_value(options: &ResolvedSandboxOptions) -> Value {
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
        let mut env_config = Map::new();
        for envvar in &options.env_passthrough {
            if is_credential_like(&envvar.0) {
                continue;
            }
            if let Ok(value) = std::env::var(&envvar.0) {
                env_config.insert(envvar.0.clone(), json!(value));
            }
        }
        for lit in &options.env_literal {
            if is_credential_like(&lit.key) {
                continue;
            }
            env_config.insert(lit.key.clone(), json!(lit.value));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::container::options::{Entrypoint, EnvLiteral, ModelFlagForm};
    use crate::engine::sandbox::options::SandboxOption;
    use std::path::PathBuf;

    fn resolve(opts: Vec<SandboxOption>) -> ResolvedSandboxOptions {
        ResolvedSandboxOptions::resolve(opts)
    }

    // ─── Schema / identity ────────────────────────────────────────────────

    #[test]
    fn includes_schema_and_agent() {
        let v =
            DSbxSessionConfig::build_value(&resolve(vec![SandboxOption::AgentId("claude".into())]));
        assert_eq!(v["schema_version"], json!(1));
        assert_eq!(v["agent"], json!("claude"));
    }

    #[test]
    fn schema_version_constant_is_one() {
        assert_eq!(SESSION_SCHEMA_VERSION, 1);
    }

    // ─── Credential exclusion ─────────────────────────────────────────────

    #[test]
    fn excludes_credential_class_env() {
        let v = DSbxSessionConfig::build_value(&resolve(vec![
            SandboxOption::EnvLiteral(EnvLiteral {
                key: "ANTHROPIC_API_KEY".into(),
                value: "sk-secret".into(),
            }),
            SandboxOption::EnvLiteral(EnvLiteral {
                key: "LOG_LEVEL".into(),
                value: "debug".into(),
            }),
        ]));
        let env = &v["env_config"];
        assert!(
            env.get("ANTHROPIC_API_KEY").is_none(),
            "credential must be excluded"
        );
        assert_eq!(env["LOG_LEVEL"], json!("debug"));
        let serialized = serde_json::to_string(&v).unwrap();
        assert!(!serialized.contains("sk-secret"));
    }

    #[test]
    fn excludes_all_mapped_credentials_from_env_config() {
        let cred_vars = [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GEMINI_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "GROQ_API_KEY",
            "MISTRAL_API_KEY",
        ];
        let opts: Vec<SandboxOption> = cred_vars
            .iter()
            .map(|k| {
                SandboxOption::EnvLiteral(EnvLiteral {
                    key: k.to_string(),
                    value: "secret-value".into(),
                })
            })
            .collect();
        let v = DSbxSessionConfig::build_value(&resolve(opts));
        let env = &v["env_config"];
        for key in &cred_vars {
            assert!(
                env.get(*key).is_none(),
                "known credential {key} must be excluded from env_config"
            );
        }
        let serialized = serde_json::to_string(&v).unwrap();
        assert!(
            !serialized.contains("secret-value"),
            "no secret value must appear in output"
        );
    }

    #[test]
    fn excludes_heuristic_credential_keys() {
        let v = DSbxSessionConfig::build_value(&resolve(vec![
            SandboxOption::EnvLiteral(EnvLiteral {
                key: "MY_SERVICE_TOKEN".into(),
                value: "tok-sensitive".into(),
            }),
            SandboxOption::EnvLiteral(EnvLiteral {
                key: "DB_PASSWORD".into(),
                value: "hunter2".into(),
            }),
            SandboxOption::EnvLiteral(EnvLiteral {
                key: "SOME_APIKEY".into(),
                value: "key123".into(),
            }),
        ]));
        let serialized = serde_json::to_string(&v).unwrap();
        assert!(!serialized.contains("tok-sensitive"));
        assert!(!serialized.contains("hunter2"));
        assert!(!serialized.contains("key123"));
    }

    // ─── Round-trip / all option variants ────────────────────────────────
    //
    // Verify that every meaningful option variant is represented in the
    // serialized JSON and survives a serialize→parse round-trip.

    #[test]
    fn round_trip_all_supported_options() {
        let opts = vec![
            SandboxOption::AgentId("gemini".into()),
            SandboxOption::Interactive(true),
            SandboxOption::EntrypointOverride(Entrypoint(vec!["/bin/sh".to_string()])),
            SandboxOption::SeededPrompt("do something".into()),
            SandboxOption::Model {
                flag: ModelFlagForm::Argument("gemini-2.0-flash".into()),
            },
            SandboxOption::SystemPromptInline {
                flag: "--system-prompt".into(),
                text: "You are a helpful assistant.".into(),
            },
            SandboxOption::SystemPromptFile {
                host_path: PathBuf::from("/host/sys.txt"),
                container_path: PathBuf::from("/container/sys.txt"),
                flag: "--sys".into(),
            },
            SandboxOption::SystemPromptEnvFile {
                env_var: "CLAUDE_SYSTEM_PROMPT".into(),
                host_path: PathBuf::from("/host/env.txt"),
                container_path: PathBuf::from("/container/env.txt"),
            },
            SandboxOption::AllowedTools(vec!["Bash".into(), "Read".into()]),
            SandboxOption::DisallowedTools(vec!["Write".into()]),
            SandboxOption::AgentSetting {
                key: "max_turns".into(),
                value: json!(10),
            },
            SandboxOption::EnvLiteral(EnvLiteral {
                key: "LOG_LEVEL".into(),
                value: "trace".into(),
            }),
        ];

        let resolved = resolve(opts);
        let value = DSbxSessionConfig::build_value(&resolved);
        let serialized = serde_json::to_string_pretty(&value).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(parsed["schema_version"], json!(1));
        assert_eq!(parsed["agent"], json!("gemini"));
        assert_eq!(parsed["interactive"], json!(true));
        assert_eq!(parsed["seeded_prompt"], json!("do something"));
        assert_eq!(parsed["model"], json!("gemini-2.0-flash"));
        assert_eq!(parsed["allowed_tools"], json!(["Bash", "Read"]));
        assert_eq!(parsed["disallowed_tools"], json!(["Write"]));
        assert_eq!(parsed["agent_settings"]["max_turns"], json!(10));
        assert_eq!(parsed["env_config"]["LOG_LEVEL"], json!("trace"));
        assert_eq!(
            parsed["system_prompt_inline"]["text"],
            json!("You are a helpful assistant.")
        );
        assert_eq!(
            parsed["system_prompt_file"]["path"],
            json!("/container/sys.txt")
        );
        assert_eq!(
            parsed["system_prompt_env_file"]["env_var"],
            json!("CLAUDE_SYSTEM_PROMPT")
        );
    }

    #[test]
    fn permission_mode_round_trips() {
        let v = DSbxSessionConfig::build_value(&resolve(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::PermissionMode("yolo".into()),
        ]));
        assert_eq!(v["permission_mode"], json!("yolo"));
    }

    #[test]
    fn mode_flags_do_not_appear_in_session_json() {
        // Argv mode flags are `kind: agent` launch args, never session.json
        // fields (the apply script has no use for raw argv strings).
        let v = DSbxSessionConfig::build_value(&resolve(vec![
            SandboxOption::AgentId("crush".into()),
            SandboxOption::AgentModeFlags(vec!["--yolo".into()]),
        ]));
        let s = serde_json::to_string(&v).unwrap();
        assert!(
            !s.contains("--yolo"),
            "mode flags must not leak into session.json"
        );
    }

    #[test]
    fn cpu_limit_does_not_appear_in_session_json() {
        // CpuLimit is recorded in ResolvedSandboxOptions but is not a config
        // field the in-VM script needs — it is surfaced as a warning to the user
        // during run_interactive. Verify it doesn't pollute session.json.
        let v = DSbxSessionConfig::build_value(&resolve(vec![
            SandboxOption::AgentId("claude".into()),
            SandboxOption::CpuLimit(4.0),
        ]));
        let s = serde_json::to_string(&v).unwrap();
        assert!(
            !s.contains("cpu"),
            "cpu_limit must not appear in session.json"
        );
        assert!(
            !s.contains("4.0"),
            "cpu value must not appear in session.json"
        );
    }

    // ─── write_for persists to filesystem ────────────────────────────────

    #[test]
    fn write_for_creates_session_json_in_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = resolve(vec![SandboxOption::AgentId("claude".into())]);
        let path = DSbxSessionConfig::write_for(&opts, tmp.path()).unwrap();
        assert!(path.exists(), "session.json must exist after write_for");
        assert_eq!(path.file_name().unwrap(), "session.json");
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["schema_version"], json!(1));
    }

    #[test]
    fn write_for_creates_awman_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = resolve(vec![]);
        DSbxSessionConfig::write_for(&opts, tmp.path()).unwrap();
        assert!(
            tmp.path().join(".awman").is_dir(),
            ".awman subdir must be created"
        );
    }
}
