//! Kit emission — renders a per-agent `spec.yaml` plus the bundled
//! `apply-session-config.sh` startup script into a kit directory.
//!
//! Called from the Layer 1 ready flow (`awman ready` for the sbx runtime).
//! Credential **values** are never written into kit files — only credential
//! mappings/config, which already live in the embedded templates.

use std::path::Path;

use crate::data::templates::{sbx_apply_script_for, sbx_kit_template_for};
use crate::engine::error::EngineError;

/// Placeholder substituted with awman's version when rendering kit templates.
const AWMAN_VERSION_PLACEHOLDER: &str = "{{AWMAN_VERSION}}";

/// Renders kit specs and startup scripts for sbx agents.
pub(super) struct DSbxKitEmitter {
    awman_version: String,
}

impl Default for DSbxKitEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl DSbxKitEmitter {
    pub(super) fn new() -> Self {
        Self {
            awman_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Render the kit for `agent` into `dest` (the kit directory):
    /// - `<dest>/spec.yaml`
    /// - `<dest>/files/home/.awman/apply-session-config.sh` (mode 0755)
    pub(super) fn emit_for_agent(&self, agent: &str, dest: &Path) -> Result<(), EngineError> {
        let template = sbx_kit_template_for(agent).ok_or_else(|| {
            EngineError::Sandbox(format!("no sbx kit template bundled for agent '{agent}'"))
        })?;
        let script = sbx_apply_script_for(agent).ok_or_else(|| {
            EngineError::Sandbox(format!("no sbx startup script bundled for agent '{agent}'"))
        })?;

        // 1. Render and write spec.yaml.
        let rendered = template.replace(AWMAN_VERSION_PLACEHOLDER, &self.awman_version);
        std::fs::create_dir_all(dest).map_err(|e| EngineError::io(dest, e))?;
        let spec_path = dest.join("spec.yaml");
        std::fs::write(&spec_path, rendered).map_err(|e| EngineError::io(&spec_path, e))?;

        // 2. Write the per-launch startup script into files/home/.awman/.
        let script_dir = dest.join("files").join("home").join(".awman");
        std::fs::create_dir_all(&script_dir).map_err(|e| EngineError::io(&script_dir, e))?;
        let script_path = script_dir.join("apply-session-config.sh");
        std::fs::write(&script_path, script).map_err(|e| EngineError::io(&script_path, e))?;
        set_executable(&script_path)?;

        Ok(())
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| EngineError::io(path, e))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| EngineError::io(path, e))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), EngineError> {
    // No executable bit on Windows; the kit's `commands.startup` invokes the
    // script via `bash`, so the mode is irrelevant there.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_spec_and_script_with_version_substituted() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("claude");
        DSbxKitEmitter::new()
            .emit_for_agent("claude", &dest)
            .unwrap();

        let spec = std::fs::read_to_string(dest.join("spec.yaml")).unwrap();
        assert!(spec.contains("kind: mixin"));
        assert!(
            !spec.contains("{{AWMAN_VERSION}}"),
            "version placeholder must be substituted"
        );
        assert!(spec.contains(env!("CARGO_PKG_VERSION")));

        let script_path = dest
            .join("files")
            .join("home")
            .join(".awman")
            .join("apply-session-config.sh");
        assert!(script_path.exists(), "startup script must be written");
        let script = std::fs::read_to_string(&script_path).unwrap();
        assert!(script.starts_with("#!/usr/bin/env bash"));
    }

    #[test]
    fn unknown_agent_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = DSbxKitEmitter::new()
            .emit_for_agent("not-an-agent", &tmp.path().join("x"))
            .unwrap_err();
        assert!(matches!(err, EngineError::Sandbox(_)));
    }

    #[cfg(unix)]
    #[test]
    fn startup_script_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("codex");
        DSbxKitEmitter::new()
            .emit_for_agent("codex", &dest)
            .unwrap();
        let script_path = dest
            .join("files")
            .join("home")
            .join(".awman")
            .join("apply-session-config.sh");
        let mode = std::fs::metadata(&script_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "script must be executable");
    }

    // ─── Golden-file shape tests: mixin agents ────────────────────────────
    //
    // Mixin kits extend a Docker built-in template and must have `kind: mixin`,
    // no `agent:` block (so no top-level `persistence`, which only exists under
    // `agent:`), a `network:` section listing the agent's API domain(s), and
    // NO `credentials:` block — the built-in kit they extend already defines
    // the well-known credential source, and sbx compose rejects a source
    // defined in both a kit and a mixin extending it. Credential values reach
    // sbx via sandbox-scoped `sbx secret set <sandbox> <service>` at launch
    // (dsbx::auth).

    fn emit_spec(agent: &str) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(agent);
        DSbxKitEmitter::new().emit_for_agent(agent, &dest).unwrap();
        std::fs::read_to_string(dest.join("spec.yaml")).unwrap()
    }

    /// The spec.yaml credential shape sbx expects: a service id keyed under
    /// `credentials.sources` whose value object lists env-var sources.
    fn credential_source_block(service: &str, env_var: &str) -> String {
        format!("{service}:\n      env:\n        - {env_var}")
    }

    /// Mixins must not redeclare the credential source their base kit already
    /// defines: sbx compose fails with `credential source "<service>" defined
    /// in both "<base>" and "awman-<agent>"`.
    fn assert_no_credentials_block(agent: &str, spec: &str) {
        assert!(
            !spec.contains("\ncredentials:"),
            "agent {agent}: mixin must not declare credentials — the base kit owns the source"
        );
    }

    #[test]
    fn claude_kit_is_mixin_without_credential_redeclaration() {
        let spec = emit_spec("claude");
        assert!(spec.contains("kind: mixin"), "claude must be kind: mixin");
        assert!(
            !spec.contains("\nagent:"),
            "mixin must not have an agent: block"
        );
        assert!(
            spec.contains("extends: claude"),
            "claude must extend the claude template"
        );
        assert!(
            !spec.contains("extends: claude-code"),
            "claude-code is not a well-known sbx agent name"
        );
        assert!(
            spec.contains("api.anthropic.com"),
            "must list anthropic domain"
        );
        assert_no_credentials_block("claude", &spec);
    }

    #[test]
    fn codex_kit_is_mixin_without_credential_redeclaration() {
        let spec = emit_spec("codex");
        assert!(spec.contains("kind: mixin"), "codex must be kind: mixin");
        assert!(
            !spec.contains("\nagent:"),
            "mixin must not have an agent: block"
        );
        assert!(
            spec.contains("extends: codex"),
            "codex must extend codex template"
        );
        assert!(spec.contains("api.openai.com"), "must list openai domain");
        assert_no_credentials_block("codex", &spec);
    }

    #[test]
    fn gemini_kit_is_mixin_without_credential_redeclaration() {
        let spec = emit_spec("gemini");
        assert!(spec.contains("kind: mixin"), "gemini must be kind: mixin");
        assert!(
            !spec.contains("\nagent:"),
            "mixin must not have an agent: block"
        );
        assert!(
            spec.contains("extends: gemini"),
            "gemini must extend gemini template"
        );
        assert!(
            spec.contains("generativelanguage.googleapis.com"),
            "must list google domain"
        );
        assert_no_credentials_block("gemini", &spec);
    }

    #[test]
    fn copilot_kit_is_mixin_without_credential_redeclaration() {
        let spec = emit_spec("copilot");
        assert!(spec.contains("kind: mixin"), "copilot must be kind: mixin");
        assert!(
            !spec.contains("\nagent:"),
            "mixin must not have an agent: block"
        );
        assert!(
            spec.contains("extends: copilot"),
            "copilot must extend copilot template"
        );
        assert!(
            spec.contains("api.githubcopilot.com"),
            "must list copilot domain"
        );
        assert_no_credentials_block("copilot", &spec);
    }

    #[test]
    fn opencode_kit_is_mixin_without_credential_redeclaration() {
        let spec = emit_spec("opencode");
        assert!(spec.contains("kind: mixin"), "opencode must be kind: mixin");
        assert!(
            !spec.contains("\nagent:"),
            "mixin must not have an agent: block"
        );
        assert!(
            spec.contains("extends: opencode"),
            "opencode must extend opencode template"
        );
        assert!(
            spec.contains("api.anthropic.com"),
            "must list anthropic domain"
        );
        assert_no_credentials_block("opencode", &spec);
    }

    // ─── Golden-file shape tests: agent kits ─────────────────────────────
    //
    // Agent kits install the agent binary themselves. They must have
    // `kind: agent`, an `agent:` block with the shell-docker base image, and a
    // `commands.install:` section with the npm installation script.

    #[test]
    fn antigravity_kit_is_agent_with_base_image_and_install() {
        let spec = emit_spec("antigravity");
        assert!(
            spec.contains("kind: agent"),
            "antigravity must be kind: agent"
        );
        assert!(
            spec.contains("docker/sandbox-templates:shell-docker"),
            "must use shell-docker base image"
        );
        assert!(spec.contains("install:"), "must have install commands");
        assert!(
            spec.contains("antigravity"),
            "install must reference the agent name"
        );
        assert!(
            spec.contains(&credential_source_block("google", "GEMINI_API_KEY")),
            "antigravity must source the google service from GEMINI_API_KEY"
        );
    }

    #[test]
    fn cline_kit_is_agent_with_base_image_and_install() {
        let spec = emit_spec("cline");
        assert!(spec.contains("kind: agent"), "cline must be kind: agent");
        assert!(
            spec.contains("docker/sandbox-templates:shell-docker"),
            "must use shell-docker base image"
        );
        assert!(spec.contains("install:"), "must have install commands");
    }

    // ─── Schema shape for all agents ─────────────────────────────────────
    //
    // sbx (v0.32+) strictly unmarshals spec.yaml: `schemaVersion` and `name`
    // are required, `persistence` only exists under `agent:`, and
    // `commands.startup` entries are objects with an argv-array `command`,
    // never bare strings.

    #[test]
    fn all_agents_have_required_schema_fields_and_structured_commands() {
        let agents = [
            "claude",
            "codex",
            "gemini",
            "copilot",
            "opencode",
            "antigravity",
            "cline",
        ];
        for agent in &agents {
            let spec = emit_spec(agent);
            assert!(
                spec.contains("schemaVersion: \"1\""),
                "agent {agent}: spec must declare schemaVersion \"1\""
            );
            assert!(
                spec.contains(&format!("name: awman-{agent}")),
                "agent {agent}: spec must declare a kit name"
            );
            assert!(
                spec.contains(
                    r#"- command: ["bash", "/home/agent/.awman/apply-session-config.sh"]"#
                ),
                "agent {agent}: startup must be an object entry with an argv-array command"
            );
            assert!(
                !spec.contains("- bash /home/agent/.awman/apply-session-config.sh"),
                "agent {agent}: startup entries must not be bare strings"
            );
            assert!(
                !spec.contains("\npersistence:"),
                "agent {agent}: persistence is only valid nested under the agent: block"
            );
            assert!(
                spec.contains("\nagentContext: |"),
                "agent {agent}: spec must carry the awman context note via agentContext"
            );
            assert!(
                !spec.contains("\nmemory:"),
                "agent {agent}: 'memory' is deprecated in kit-spec v2 (sbx warns at \
                 compose); use 'agentContext'"
            );
        }
    }

    /// Parse every emitted spec as YAML and assert the typed shapes sbx's
    /// strict unmarshaller requires — the string-level checks above can't
    /// catch indentation mistakes that change the parsed structure.
    #[test]
    fn all_agents_specs_parse_with_sbx_compatible_types() {
        let agents = [
            "claude",
            "codex",
            "gemini",
            "copilot",
            "opencode",
            "antigravity",
            "cline",
        ];
        for agent in &agents {
            let spec = emit_spec(agent);
            let doc: serde_yaml::Value = serde_yaml::from_str(&spec)
                .unwrap_or_else(|e| panic!("agent {agent}: invalid YAML: {e}"));

            assert_eq!(
                doc["schemaVersion"].as_str(),
                Some("1"),
                "agent {agent}: schemaVersion must be the string \"1\""
            );
            assert!(doc["name"].is_string(), "agent {agent}: name is required");
            assert!(
                doc.get("persistence").is_none(),
                "agent {agent}: persistence must not be a top-level field"
            );

            match doc["kind"].as_str() {
                Some("mixin") => {
                    assert!(
                        doc["extends"].is_string(),
                        "agent {agent}: mixin must declare extends"
                    );
                    assert!(
                        doc.get("agent").is_none(),
                        "agent {agent}: mixin must not have an agent block"
                    );
                    // The extended built-in kit defines the well-known credential
                    // source; sbx compose rejects a source defined in both.
                    assert!(
                        doc.get("credentials").is_none(),
                        "agent {agent}: mixin must not redeclare credential sources"
                    );
                }
                Some("agent") => {
                    assert!(
                        doc["agent"]["image"].is_string(),
                        "agent {agent}: agent.image is required"
                    );
                    assert_eq!(
                        doc["agent"]["persistence"].as_str(),
                        Some("persistent"),
                        "agent {agent}: agent kits must be persistent (WI 0090)"
                    );
                    let sources = doc["credentials"]["sources"]
                        .as_mapping()
                        .unwrap_or_else(|| {
                            panic!("agent {agent}: credentials.sources must be a mapping")
                        });
                    for (service, source) in sources {
                        assert!(
                            source["env"].is_sequence(),
                            "agent {agent}: credential source {service:?} must be an object with an env list"
                        );
                    }
                }
                other => panic!("agent {agent}: unexpected kind {other:?}"),
            }

            for entry in doc["commands"]["startup"]
                .as_sequence()
                .unwrap_or_else(|| panic!("agent {agent}: commands.startup must be a list"))
            {
                assert!(
                    entry["command"].is_sequence(),
                    "agent {agent}: startup command must be an argv array, not a shell string"
                );
            }
            if let Some(install) = doc["commands"].get("install") {
                for entry in install.as_sequence().unwrap() {
                    assert!(
                        entry["command"].is_string(),
                        "agent {agent}: install command must be a shell string"
                    );
                }
            }
        }
    }

    // ─── Version substitution for all agents ─────────────────────────────

    #[test]
    fn all_agents_have_version_substituted() {
        let agents = [
            "claude",
            "codex",
            "gemini",
            "copilot",
            "opencode",
            "antigravity",
            "cline",
        ];
        for agent in &agents {
            let spec = emit_spec(agent);
            assert!(
                !spec.contains("{{AWMAN_VERSION}}"),
                "agent {agent}: version placeholder must be substituted"
            );
            assert!(
                spec.contains(env!("CARGO_PKG_VERSION")),
                "agent {agent}: current version must appear in spec.yaml"
            );
        }
    }

    // ─── All agents emit the startup script with executable mode ─────────

    #[cfg(unix)]
    #[test]
    fn all_agents_startup_script_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let agents = [
            "claude",
            "codex",
            "gemini",
            "copilot",
            "opencode",
            "antigravity",
            "cline",
        ];
        for agent in &agents {
            let tmp = tempfile::tempdir().unwrap();
            let dest = tmp.path().join(agent);
            DSbxKitEmitter::new().emit_for_agent(agent, &dest).unwrap();
            let script = dest
                .join("files")
                .join("home")
                .join(".awman")
                .join("apply-session-config.sh");
            assert!(
                script.exists(),
                "agent {agent}: startup script must exist at files/home/.awman/apply-session-config.sh"
            );
            let mode = std::fs::metadata(&script).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "agent {agent}: startup script must be executable (mode={mode:o})"
            );
        }
    }

    #[test]
    fn all_agents_startup_script_starts_with_shebang() {
        let agents = [
            "claude",
            "codex",
            "gemini",
            "copilot",
            "opencode",
            "antigravity",
            "cline",
        ];
        for agent in &agents {
            let tmp = tempfile::tempdir().unwrap();
            let dest = tmp.path().join(agent);
            DSbxKitEmitter::new().emit_for_agent(agent, &dest).unwrap();
            let script = std::fs::read_to_string(
                dest.join("files")
                    .join("home")
                    .join(".awman")
                    .join("apply-session-config.sh"),
            )
            .unwrap();
            assert!(
                script.starts_with("#!/"),
                "agent {agent}: startup script must start with a shebang"
            );
        }
    }
}
