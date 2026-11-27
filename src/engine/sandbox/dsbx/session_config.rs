//! Per-launch policy rendering (contract §6.3) and the `sbx run` session
//! environment (contract §5.6).
//!
//! awman no longer writes `<workspace>/.awman/session.json`: nothing is
//! written into the workspace. Instead the host renders the agent's managed
//! guest files into an [`ApplyDocument`] that `awman-apply` writes and
//! acknowledges before the agent starts. The document has no field for a
//! prompt, an environment value or a credential, so none can leak through it.
//! An existing `session.json` from an older awman is left untouched.
//!
//! The session environment is a list of non-secret `KEY=value` pairs set on
//! the `sbx run` child process; only the key names appear in its argv.

use serde_json::{Map, Value};

use crate::engine::agent::sbx_contract::{
    ApplyDocument, ApplyFile, SbxRoute, CLAUDE_MANAGED_SETTINGS, CLAUDE_MEMORY, CODEX_AGENTS_MD,
    COPILOT_INSTRUCTIONS, LAUNCH_ID_ENV, OPENCODE_AGENTS_MD,
};
use crate::engine::container::host_env_denylist::scrub_run_env;
use crate::engine::container::options::ModelFlagForm;
use crate::engine::error::EngineError;
use crate::engine::sandbox::dsbx::auth::is_credential_like;
use crate::engine::sandbox::options::ResolvedSandboxOptions;

/// Prefix of codex's inline system prompt (`--config developer_instructions=…`),
/// removed when the text becomes `~/.codex/AGENTS.md`.
const CODEX_INSTRUCTIONS_PREFIX: &str = "developer_instructions=";

/// The notice for host-terminal variables the options asked for that are
/// not passed to the sandbox session, or `None`.
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

/// Render this launch's apply document. Every managed file of the route is
/// either present (rewritten) or absent (removed in the guest), so no policy
/// from an earlier launch survives.
pub(super) fn render_apply_document(
    route: &SbxRoute,
    kit_digest: &str,
    launch_id: &str,
    options: &ResolvedSandboxOptions,
) -> Result<ApplyDocument, EngineError> {
    let prompt = options
        .system_prompt_inline
        .as_ref()
        .map(|(_flag, text)| text.as_str());
    let mut files = Vec::new();
    for managed in route.managed_files {
        let content = match managed.id {
            CLAUDE_MANAGED_SETTINGS => Some(claude_managed_settings(options)?),
            CLAUDE_MEMORY | OPENCODE_AGENTS_MD | COPILOT_INSTRUCTIONS => {
                prompt.map(|text| with_newline(text.to_string()))
            }
            CODEX_AGENTS_MD => prompt.map(|text| {
                with_newline(
                    text.strip_prefix(CODEX_INSTRUCTIONS_PREFIX)
                        .unwrap_or(text)
                        .to_string(),
                )
            }),
            other => {
                return Err(EngineError::Sandbox(format!(
                    "no renderer for managed file '{other}' of agent '{}'",
                    route.agent
                )))
            }
        };
        if let Some(content) = content {
            files.push(ApplyFile {
                id: managed.id.to_string(),
                content,
            });
        }
    }
    Ok(ApplyDocument {
        launch_id: launch_id.to_string(),
        agent: route.agent.to_string(),
        kit_digest: kit_digest.to_string(),
        files,
    })
}

fn with_newline(mut text: String) -> Vec<u8> {
    text.push('\n');
    text.into_bytes()
}

/// `/etc/claude-code/managed-settings.json`: the user's `agentSettings`
/// for claude, overridden by this launch's model, mode and tool lists.
fn claude_managed_settings(options: &ResolvedSandboxOptions) -> Result<Vec<u8>, EngineError> {
    let mut root = match options.agent_settings.get("claude") {
        None => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            return Err(EngineError::Config(
                "agentSettings for 'claude' must be a JSON object to be delivered on Docker \
                 Sandboxes"
                    .into(),
            ))
        }
    };
    if let Some(model) = &options.model {
        let model = match model {
            ModelFlagForm::Argument(name) => name.clone(),
            ModelFlagForm::Shorthand(s) => s.clone(),
        };
        root.insert("model".into(), Value::String(model));
    }
    let mode = options.permission_mode.as_deref();
    let mut permissions = match root.remove("permissions") {
        Some(Value::Object(map)) => map,
        None => Map::new(),
        Some(_) => {
            return Err(EngineError::Config(
                "agentSettings.claude.permissions must be a JSON object".into(),
            ))
        }
    };
    match mode {
        None => {}
        Some("plan") => {
            permissions.insert("defaultMode".into(), "plan".into());
        }
        Some("auto") => {
            permissions.insert("defaultMode".into(), "acceptEdits".into());
        }
        Some("yolo") => {
            permissions.insert("defaultMode".into(), "bypassPermissions".into());
        }
        Some(other) => {
            return Err(EngineError::Config(format!(
                "unknown permission mode '{other}' for Docker Sandboxes"
            )))
        }
    }
    if !options.allowed_tools.is_empty() {
        permissions.insert("allow".into(), options.allowed_tools.clone().into());
    }
    if !options.disallowed_tools.is_empty() {
        permissions.insert("deny".into(), options.disallowed_tools.clone().into());
    }
    if mode == Some("yolo") {
        permissions.remove("disableBypassPermissionsMode");
        root.insert(
            "skipDangerousModePermissionPrompt".into(),
            Value::Bool(true),
        );
    } else {
        permissions.insert("disableBypassPermissionsMode".into(), "disable".into());
        root.remove("skipDangerousModePermissionPrompt");
    }
    root.insert("permissions".into(), Value::Object(permissions));
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(root))
        .map_err(|e| EngineError::Sandbox(format!("serialize claude managed settings: {e}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// The `sbx run` session environment: `AWMAN_LAUNCH_ID`, the route's fixed
/// `session_env`, then every requested non-credential variable that
/// survives the host-terminal scrub. Credential-like names are never passed
/// (they reach the agent only as sandbox-scoped `sbx secret set` secrets).
pub(super) fn run_environment(
    route: &SbxRoute,
    launch_id: &str,
    options: &ResolvedSandboxOptions,
) -> Vec<(String, String)> {
    let snapshot = options
        .host_env
        .clone()
        .unwrap_or_else(crate::engine::host_cli::process_env);
    let mut requested: Vec<(String, String)> = Vec::new();
    for envvar in &options.env_passthrough {
        if is_credential_like(&envvar.0) {
            continue;
        }
        if let Some(value) = snapshot.get(&envvar.0) {
            requested.push((envvar.0.clone(), value.to_owned()));
        }
    }
    for lit in &options.env_literal {
        if is_credential_like(&lit.key) {
            continue;
        }
        requested.push((lit.key.clone(), lit.value.clone()));
    }
    let mut env = vec![(LAUNCH_ID_ENV.to_string(), launch_id.to_string())];
    for (key, value) in route.session_env {
        env.push((key.to_string(), value.to_string()));
    }
    for (key, value) in scrub_run_env(requested, snapshot.get("COLORTERM")).kept {
        if env.iter().any(|(k, _)| *k == key) {
            continue;
        }
        env.push((key, value));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::agent::sbx_contract::sbx_route;
    use crate::engine::container::options::{EnvLiteral, EnvVar};
    use crate::engine::sandbox::options::SandboxOption;

    fn settings(document: &ApplyDocument) -> Value {
        let file = document
            .files
            .iter()
            .find(|f| f.id == CLAUDE_MANAGED_SETTINGS)
            .expect("claude managed settings are always present");
        serde_json::from_slice(&file.content).unwrap()
    }

    #[test]
    fn claude_settings_carry_mode_tools_and_model_and_forbid_bypass_unless_yolo() {
        let route = sbx_route("claude").unwrap();
        let options = ResolvedSandboxOptions::resolve([
            SandboxOption::AgentId("claude".into()),
            SandboxOption::PermissionMode("plan".into()),
            SandboxOption::AllowedTools(vec!["Read".into()]),
            SandboxOption::DisallowedTools(vec!["Bash".into()]),
            SandboxOption::Model {
                flag: ModelFlagForm::Argument("opus".into()),
            },
            SandboxOption::AgentSetting {
                key: "claude".into(),
                value: serde_json::json!({"theme": "dark", "skipDangerousModePermissionPrompt": true}),
            },
        ]);
        let document = render_apply_document(&route, "ab", "l-1", &options).unwrap();
        assert_eq!(document.agent, "claude");
        assert_eq!(document.files.len(), 1, "no system prompt, no CLAUDE.md");
        let value = settings(&document);
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["model"], "opus");
        assert_eq!(value["permissions"]["defaultMode"], "plan");
        assert_eq!(value["permissions"]["allow"], serde_json::json!(["Read"]));
        assert_eq!(value["permissions"]["deny"], serde_json::json!(["Bash"]));
        assert_eq!(
            value["permissions"]["disableBypassPermissionsMode"],
            "disable"
        );
        assert!(value.get("skipDangerousModePermissionPrompt").is_none());

        let yolo = ResolvedSandboxOptions::resolve([SandboxOption::PermissionMode("yolo".into())]);
        let value = settings(&render_apply_document(&route, "ab", "l-1", &yolo).unwrap());
        assert_eq!(value["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(value["skipDangerousModePermissionPrompt"], true);
        assert!(value["permissions"]
            .get("disableBypassPermissionsMode")
            .is_none());

        let auto = ResolvedSandboxOptions::resolve([SandboxOption::PermissionMode("auto".into())]);
        let value = settings(&render_apply_document(&route, "ab", "l-1", &auto).unwrap());
        assert_eq!(value["permissions"]["defaultMode"], "acceptEdits");

        // With nothing requested, bypass is still disabled.
        let none = ResolvedSandboxOptions::default();
        let value = settings(&render_apply_document(&route, "ab", "l-1", &none).unwrap());
        assert!(value["permissions"].get("defaultMode").is_none());
        assert_eq!(
            value["permissions"]["disableBypassPermissionsMode"],
            "disable"
        );

        let bad = ResolvedSandboxOptions::resolve([SandboxOption::AgentSetting {
            key: "claude".into(),
            value: serde_json::json!(["not", "an", "object"]),
        }]);
        assert!(matches!(
            render_apply_document(&route, "ab", "l-1", &bad),
            Err(EngineError::Config(_))
        ));
    }

    #[test]
    fn system_prompts_become_the_routes_managed_files() {
        let prompt = |text: &str| {
            ResolvedSandboxOptions::resolve([SandboxOption::SystemPromptInline {
                flag: "--config".into(),
                text: text.into(),
            }])
        };
        let codex = render_apply_document(
            &sbx_route("codex").unwrap(),
            "ab",
            "l",
            &prompt("developer_instructions=be brief"),
        )
        .unwrap();
        assert_eq!(codex.files.len(), 1);
        assert_eq!(codex.files[0].id, CODEX_AGENTS_MD);
        assert_eq!(codex.files[0].content, b"be brief\n");
        let opencode =
            render_apply_document(&sbx_route("opencode").unwrap(), "ab", "l", &prompt("ctx"))
                .unwrap();
        assert_eq!(opencode.files[0].id, OPENCODE_AGENTS_MD);
        let copilot =
            render_apply_document(&sbx_route("copilot").unwrap(), "ab", "l", &prompt("ctx"))
                .unwrap();
        assert_eq!(copilot.files[0].id, COPILOT_INSTRUCTIONS);
        let claude =
            render_apply_document(&sbx_route("claude").unwrap(), "ab", "l", &prompt("ctx"))
                .unwrap();
        assert!(claude
            .files
            .iter()
            .any(|f| f.id == CLAUDE_MEMORY && f.content == b"ctx\n"));
        // No prompt this launch: the file is absent, so the guest removes it.
        let empty = render_apply_document(
            &sbx_route("codex").unwrap(),
            "ab",
            "l",
            &ResolvedSandboxOptions::default(),
        )
        .unwrap();
        assert!(empty.files.is_empty());
        // Cline has no managed files; its prompt travels in argv.
        let cline =
            render_apply_document(&sbx_route("cline").unwrap(), "ab", "l", &prompt("ctx")).unwrap();
        assert!(cline.files.is_empty());
    }

    #[test]
    fn apply_document_and_session_env_never_carry_credentials_or_prompts() {
        let snapshot = crate::data::config::env::EnvSnapshot::with_overrides([
            ("ANTHROPIC_API_KEY", "sk-ant-secret-1"),
            ("MY_SERVICE_TOKEN", "tok-secret-2"),
            ("PLAIN", "plain-value"),
            ("TERM_PROGRAM", "host-terminal"),
        ]);
        let options = ResolvedSandboxOptions {
            host_env: Some(snapshot),
            ..ResolvedSandboxOptions::resolve([
                SandboxOption::AgentId("claude".into()),
                SandboxOption::EnvPassthrough(EnvVar("ANTHROPIC_API_KEY".into())),
                SandboxOption::EnvPassthrough(EnvVar("MY_SERVICE_TOKEN".into())),
                SandboxOption::EnvPassthrough(EnvVar("PLAIN".into())),
                SandboxOption::EnvPassthrough(EnvVar("TERM_PROGRAM".into())),
                SandboxOption::EnvLiteral(EnvLiteral {
                    key: "DB_PASSWORD".into(),
                    value: "pw-secret-3".into(),
                }),
                SandboxOption::EnvLiteral(EnvLiteral {
                    key: "LITERAL".into(),
                    value: "lit-value".into(),
                }),
                SandboxOption::AgentCredentials {
                    env_vars: vec![("OPENAI_API_KEY".into(), "sk-secret-4".into())],
                },
                SandboxOption::SeededPrompt("seeded-prompt-text".into()),
            ])
        };
        let route = sbx_route("claude").unwrap();
        let encoded = render_apply_document(&route, "ab", "l-1", &options)
            .unwrap()
            .encode()
            .unwrap();
        let text = String::from_utf8(encoded).unwrap();
        for secret in [
            "sk-ant-secret-1",
            "tok-secret-2",
            "pw-secret-3",
            "sk-secret-4",
            "seeded-prompt-text",
            "plain-value",
            "lit-value",
        ] {
            assert!(
                !text.contains(secret),
                "{secret} leaked into the apply wire"
            );
        }

        let env = run_environment(&route, "l-1", &options);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        // TERM is forced by the same terminal scrub every backend applies.
        assert_eq!(keys, ["AWMAN_LAUNCH_ID", "PLAIN", "LITERAL", "TERM"]);
        assert_eq!(env[0].1, "l-1");
        for (_, value) in &env {
            assert!(!value.contains("secret"), "{value}");
        }
        let copilot = run_environment(
            &sbx_route("copilot").unwrap(),
            "l-2",
            &ResolvedSandboxOptions::default(),
        );
        assert!(copilot
            .iter()
            .any(|(k, v)| k == "COPILOT_CUSTOM_INSTRUCTIONS_DIRS"
                && v == "/home/agent/.awman/copilot-instructions"));
    }
}
