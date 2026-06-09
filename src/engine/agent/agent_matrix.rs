//! Per-agent translation matrix — the only place in `src/engine/` that
//! branches on agent name. Adding a new agent is a single-file edit here.

use crate::engine::container::options::{Entrypoint, ModelFlagForm};
use crate::engine::error::EngineError;

/// Supported agent names — derived from the legacy `Agent` enum in
/// `oldsrc/cli.rs`.
pub const SUPPORTED_AGENTS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "maki",
    "gemini",
    "copilot",
    "crush",
    "cline",
    "antigravity",
];

/// How awman injects the combined context system prompt into an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPromptMode {
    /// Append to default system prompt by mounting the prompt as a file and
    /// referencing it with a CLI flag (e.g. claude `--append-system-prompt-file <path>`).
    Append,
    /// Append to default system prompt by passing it inline as `<key>=<text>`
    /// to a CLI flag (e.g. codex `--config developer_instructions=<text>`).
    AppendInline { key: &'static str },
    /// Replace default system prompt with inline text (destructive; preamble
    /// prepended). Used by agents like cline `--system <text>`.
    Replace,
    /// File-based: write AGENTS.md into context dir.
    AgentsMd,
    /// Env var pointing to a file.
    EnvFile { var: &'static str },
    /// Extra workspace dir flag (agy --add-dir) + AGENTS.md.
    AddDir { flag: &'static str },
    /// Not supported.
    Unsupported,
}

/// Per-agent metadata used by `AgentEngine::build_options`.
#[derive(Debug, Clone)]
pub struct AgentMatrix {
    pub agent: &'static str,
    /// Bare interactive entrypoint (e.g. `["claude"]`, `["copilot", "-i"]`).
    pub interactive_entrypoint: Vec<&'static str>,
    /// Print/non-interactive entrypoint suffix (e.g. `--print` for Claude).
    pub non_interactive_flag: Option<&'static str>,
    /// Whether plan mode is supported and which flag to emit.
    pub plan_flag: Option<&'static [&'static str]>,
    /// Yolo flag (e.g. `--dangerously-skip-permissions`). `None` means yolo
    /// silently equates to no permission flags.
    pub yolo_flag: Option<&'static str>,
    /// Auto flag (e.g. `--permission-mode auto`).
    pub auto_flag: Option<&'static [&'static str]>,
    /// Disallowed-tools flag name (e.g. `--disallowedTools`).
    pub disallowed_tools_flag: Option<&'static str>,
    /// Allowed-tools flag name (e.g. `--allowedTools`).
    pub allowed_tools_flag: Option<&'static str>,
    /// How model is delivered (`--model NAME` for most).
    pub model_flag: ModelFlagDelivery,
    /// Whether the agent supports mid-session prompt injection over its
    /// already-running container's stdin. Used by the workflow engine to
    /// decide between reusing a long-lived container (when `true`) and
    /// spinning up a fresh one per step (when `false`).
    ///
    /// Currently `false` for every shipped agent. Set to `true` once an agent
    /// CLI is verified to accept a newline-terminated prompt on its existing
    /// stdin without losing state. The wiring on the Docker side keeps the
    /// spawned subprocess's stdin alive for re-injection.
    pub supports_stdin_injection: bool,
    /// How context system prompts are delivered to this agent.
    pub system_prompt_delivery: SystemPromptMode,
    /// CLI flag for system prompt delivery (e.g. `--append-system-prompt-file`).
    pub system_prompt_flag: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub enum ModelFlagDelivery {
    /// `--model NAME`
    SpaceArg,
    /// `--model=NAME`
    EqArg,
    /// Not supported.
    Unsupported,
}

/// Lookup the matrix entry for a known agent name.
pub fn matrix_for(agent: &str) -> Result<AgentMatrix, EngineError> {
    Ok(match agent {
        "claude" => AgentMatrix {
            agent: "claude",
            interactive_entrypoint: vec!["claude"],
            non_interactive_flag: Some("--print"),
            plan_flag: Some(&["--permission-mode", "plan"]),
            yolo_flag: Some("--dangerously-skip-permissions"),
            auto_flag: Some(&["--permission-mode", "auto"]),
            disallowed_tools_flag: Some("--disallowedTools"),
            allowed_tools_flag: Some("--allowedTools"),
            model_flag: ModelFlagDelivery::SpaceArg,
            supports_stdin_injection: false,
            system_prompt_delivery: SystemPromptMode::Append,
            system_prompt_flag: Some("--append-system-prompt-file"),
        },
        "codex" => AgentMatrix {
            agent: "codex",
            interactive_entrypoint: vec!["codex"],
            non_interactive_flag: Some("exec"),
            plan_flag: Some(&["--approval-mode", "plan"]),
            yolo_flag: Some("--full-auto"),
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            supports_stdin_injection: false,
            // codex takes the system prompt as `--config developer_instructions=<text>`.
            system_prompt_delivery: SystemPromptMode::AppendInline {
                key: "developer_instructions",
            },
            system_prompt_flag: Some("--config"),
        },
        "opencode" => AgentMatrix {
            agent: "opencode",
            interactive_entrypoint: vec!["opencode"],
            non_interactive_flag: Some("run"),
            plan_flag: None,
            yolo_flag: None,
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            supports_stdin_injection: false,
            system_prompt_delivery: SystemPromptMode::AgentsMd,
            system_prompt_flag: None,
        },
        "maki" => AgentMatrix {
            agent: "maki",
            interactive_entrypoint: vec!["maki"],
            non_interactive_flag: None,
            plan_flag: None,
            yolo_flag: Some("--yolo"),
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            supports_stdin_injection: false,
            system_prompt_delivery: SystemPromptMode::Unsupported,
            system_prompt_flag: None,
        },
        "gemini" => AgentMatrix {
            agent: "gemini",
            interactive_entrypoint: vec!["gemini"],
            non_interactive_flag: None,
            plan_flag: Some(&["--approval-mode=plan"]),
            yolo_flag: Some("--yolo"),
            auto_flag: Some(&["--approval-mode=auto_edit"]),
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            supports_stdin_injection: false,
            system_prompt_delivery: SystemPromptMode::EnvFile {
                var: "GEMINI_SYSTEM_MD",
            },
            system_prompt_flag: None,
        },
        "copilot" => AgentMatrix {
            agent: "copilot",
            interactive_entrypoint: vec!["copilot", "-i"],
            non_interactive_flag: None,
            plan_flag: Some(&["--plan"]),
            yolo_flag: Some("--autopilot"),
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            supports_stdin_injection: false,
            system_prompt_delivery: SystemPromptMode::EnvFile {
                var: "COPILOT_CUSTOM_INSTRUCTIONS_DIRS",
            },
            system_prompt_flag: None,
        },
        "crush" => AgentMatrix {
            agent: "crush",
            interactive_entrypoint: vec!["crush"],
            non_interactive_flag: Some("run"),
            plan_flag: None,
            yolo_flag: Some("--yolo"),
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            supports_stdin_injection: false,
            system_prompt_delivery: SystemPromptMode::Unsupported,
            system_prompt_flag: None,
        },
        "cline" => AgentMatrix {
            agent: "cline",
            interactive_entrypoint: vec!["cline"],
            non_interactive_flag: Some("task"),
            plan_flag: Some(&["--plan"]),
            yolo_flag: Some("--yolo"),
            auto_flag: Some(&["--auto-approve-all"]),
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::SpaceArg,
            supports_stdin_injection: false,
            system_prompt_delivery: SystemPromptMode::Replace,
            system_prompt_flag: Some("--system"),
        },
        "antigravity" => AgentMatrix {
            // Verified against `agy --help` (v1.0.x). Flags actually accepted:
            //   --print / -p / --prompt           (non-interactive)
            //   --prompt-interactive / -i         (interactive seed)
            //   --dangerously-skip-permissions    (yolo)
            //   --print-timeout                   (default 5m, not surfaced here)
            //   --continue / --conversation       (session resume, not wired)
            //   --add-dir                         (extra workspace dirs)
            //   --log-file, --sandbox
            // There is **no** `--approval-mode` / `--plan` / `--auto-edit`
            // CLI flag — those are settings.json (`toolPermission`) values
            // surfaced through agy's interactive `/...` slash commands.
            // Don't emit them; the binary just dumps `--help` and treats the
            // prompt as the agy executable name. Leaving plan/auto as `None`
            // keeps non-yolo modes a silent no-op (matches opencode/maki).
            agent: "antigravity",
            interactive_entrypoint: vec!["agy"],
            non_interactive_flag: Some("--print"),
            plan_flag: None,
            yolo_flag: Some("--dangerously-skip-permissions"),
            auto_flag: None,
            disallowed_tools_flag: None,
            allowed_tools_flag: None,
            model_flag: ModelFlagDelivery::Unsupported,
            supports_stdin_injection: false,
            system_prompt_delivery: SystemPromptMode::AddDir { flag: "--add-dir" },
            system_prompt_flag: None,
        },
        other => {
            return Err(EngineError::Other(format!(
                "unknown agent '{other}'; supported: {}",
                SUPPORTED_AGENTS.join(", ")
            )))
        }
    })
}

/// Build the entrypoint with optional non-interactive shape.
pub fn entrypoint_for(matrix: &AgentMatrix, non_interactive: bool) -> Entrypoint {
    let mut parts: Vec<String> = matrix
        .interactive_entrypoint
        .iter()
        .map(|s| s.to_string())
        .collect();
    if non_interactive {
        if let Some(flag) = matrix.non_interactive_flag {
            // For agents like Codex (`codex exec`) the "flag" is actually a
            // subcommand inserted after the binary; for Claude it's `--print`
            // appended after the args. Both append-at-end shapes work here
            // because the seeded prompt is positional.
            parts.push(flag.to_string());
        }
    }
    Entrypoint(parts)
}

/// Translate a model name into the matrix-specific flag form.
pub fn model_flag_for(matrix: &AgentMatrix, model: &str) -> Result<ModelFlagForm, EngineError> {
    match matrix.model_flag {
        ModelFlagDelivery::SpaceArg => Ok(ModelFlagForm::Argument(model.to_string())),
        ModelFlagDelivery::EqArg => Ok(ModelFlagForm::Argument(format!("--model={model}"))),
        ModelFlagDelivery::Unsupported => Err(EngineError::Other(format!(
            "agent '{}' does not support a model flag",
            matrix.agent
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_supports_all_agents() {
        for a in SUPPORTED_AGENTS {
            assert!(matrix_for(a).is_ok(), "matrix missing for {a}");
        }
    }

    #[test]
    fn unknown_agent_errors() {
        assert!(matrix_for("totallymade-up").is_err());
    }

    #[test]
    fn opencode_plan_unsupported() {
        let m = matrix_for("opencode").unwrap();
        assert!(m.plan_flag.is_none());
    }

    #[test]
    fn antigravity_yolo_flag_is_dangerously_skip_permissions() {
        let m = matrix_for("antigravity").unwrap();
        assert_eq!(
            m.yolo_flag,
            Some("--dangerously-skip-permissions"),
            "antigravity yolo_flag must be --dangerously-skip-permissions"
        );
    }

    #[test]
    fn antigravity_non_interactive_flag_is_print() {
        let m = matrix_for("antigravity").unwrap();
        assert_eq!(
            m.non_interactive_flag,
            Some("--print"),
            "antigravity non_interactive_flag must be --print"
        );
    }

    #[test]
    fn antigravity_model_flag_unsupported_returns_err() {
        let m = matrix_for("antigravity").unwrap();
        let result = model_flag_for(&m, "gemini-3.5-flash");
        assert!(
            result.is_err(),
            "model_flag_for antigravity must return Err (Unsupported); got {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("antigravity"),
            "error must name the agent; got: {msg}"
        );
        assert!(
            msg.contains("does not support a model flag"),
            "error must say 'does not support a model flag'; got: {msg}"
        );
    }

    #[test]
    fn antigravity_interactive_entrypoint_is_agy() {
        let m = matrix_for("antigravity").unwrap();
        assert_eq!(
            m.interactive_entrypoint,
            vec!["agy"],
            "antigravity interactive_entrypoint must be [\"agy\"]"
        );
    }

    // ─── WI-0087: system prompt delivery matrix ────────────────────────────────

    #[test]
    fn all_supported_agents_have_valid_system_prompt_delivery() {
        for agent in SUPPORTED_AGENTS {
            let m = matrix_for(agent).expect("matrix must exist for all SUPPORTED_AGENTS");
            // Just constructing the matrix (above) validates no panic.
            // Additionally verify the delivery is a valid variant by matching it.
            let _valid = match &m.system_prompt_delivery {
                SystemPromptMode::Append
                | SystemPromptMode::AppendInline { .. }
                | SystemPromptMode::Replace
                | SystemPromptMode::AgentsMd
                | SystemPromptMode::EnvFile { .. }
                | SystemPromptMode::AddDir { .. }
                | SystemPromptMode::Unsupported => true,
            };
        }
    }

    #[test]
    fn codex_system_prompt_delivery_is_append_inline_with_developer_instructions() {
        let m = matrix_for("codex").unwrap();
        assert!(
            matches!(
                m.system_prompt_delivery,
                SystemPromptMode::AppendInline {
                    key: "developer_instructions"
                }
            ),
            "codex must use AppendInline {{ key: developer_instructions }}; got {:?}",
            m.system_prompt_delivery
        );
        assert_eq!(
            m.system_prompt_flag,
            Some("--config"),
            "codex system_prompt_flag must be --config"
        );
    }

    #[test]
    fn claude_system_prompt_delivery_is_append() {
        let m = matrix_for("claude").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::Append,
            "claude must use Append delivery"
        );
        assert_eq!(
            m.system_prompt_flag,
            Some("--append-system-prompt-file"),
            "claude system_prompt_flag must be --append-system-prompt-file"
        );
    }

    #[test]
    fn maki_system_prompt_delivery_is_unsupported() {
        let m = matrix_for("maki").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::Unsupported,
            "maki must use Unsupported delivery"
        );
        assert!(
            m.system_prompt_flag.is_none(),
            "maki must have no system_prompt_flag"
        );
    }

    #[test]
    fn cline_system_prompt_delivery_is_replace() {
        let m = matrix_for("cline").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::Replace,
            "cline must use Replace delivery"
        );
        assert_eq!(
            m.system_prompt_flag,
            Some("--system"),
            "cline system_prompt_flag must be --system"
        );
    }

    #[test]
    fn crush_system_prompt_delivery_is_unsupported() {
        let m = matrix_for("crush").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::Unsupported,
            "crush must use Unsupported delivery"
        );
    }

    #[test]
    fn antigravity_system_prompt_delivery_is_add_dir() {
        let m = matrix_for("antigravity").unwrap();
        assert!(
            matches!(
                m.system_prompt_delivery,
                SystemPromptMode::AddDir { flag: "--add-dir" }
            ),
            "antigravity must use AddDir {{ flag: \"--add-dir\" }}; got {:?}",
            m.system_prompt_delivery
        );
    }

    #[test]
    fn opencode_system_prompt_delivery_is_agents_md() {
        let m = matrix_for("opencode").unwrap();
        assert_eq!(
            m.system_prompt_delivery,
            SystemPromptMode::AgentsMd,
            "opencode must use AgentsMd delivery"
        );
    }
}
