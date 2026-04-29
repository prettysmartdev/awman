use crate::commands::agent::{append_autonomous_flags, prepare_agent_cli, run_agent_with_sink};
use crate::commands::auth::resolve_auth;
use crate::commands::implement::confirm_mount_scope_stdin;
use crate::commands::init_flow::find_git_root;
use crate::commands::output::OutputSink;
use crate::config::{effective_env_passthrough, effective_yolo_disallowed_tools, load_repo_config};
use crate::runtime::HostSettings;
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Command-mode entry point for `amux chat`.
pub async fn run(non_interactive: bool, plan: bool, allow_docker: bool, mount_ssh: bool, yolo: bool, auto: bool, agent_override: Option<String>, model_override: Option<String>, raw_overlay_flags: &[String], runtime: std::sync::Arc<dyn crate::runtime::AgentRuntime>) -> Result<()> {
    let git_root = find_git_root().context("Not inside a Git repository")?;
    let mount_path = confirm_mount_scope_stdin(&git_root)?;
    let config = load_repo_config(&git_root)?;
    let config_agent = config.agent.as_deref().unwrap_or("claude").to_string();
    let agent = agent_override.as_deref().unwrap_or(&config_agent).to_string();
    let credentials = resolve_auth(&git_root, &agent)?;
    let host_settings = crate::passthrough::passthrough_for_agent(&agent).prepare_host_settings();

    // Suppress the dangerous-mode permission dialog when running with --yolo.
    if yolo {
        if let Some(ref s) = host_settings {
            let _ = s.apply_yolo_settings();
        }
    }

    let mut env_vars = credentials.env_vars.clone();
    let passthrough_names = effective_env_passthrough(&git_root);
    for name in &passthrough_names {
        // Skip vars already supplied by keychain credentials — keychain takes precedence.
        if env_vars.iter().any(|(k, _)| k == name) {
            continue;
        }
        if let Ok(val) = std::env::var(name) {
            env_vars.push((name.clone(), val));
        }
    }

    // Ensure the requested agent is available; offer fallback to default if setup is declined.
    let effective_agent = prepare_agent_cli(&git_root, &agent, &config_agent, &*runtime).await?;

    // Recompute credentials and env_vars if fallback changed the agent.
    let (final_env_vars, mut final_host_settings) = if effective_agent != agent {
        let new_creds = resolve_auth(&git_root, &effective_agent)?;
        let new_hs = crate::passthrough::passthrough_for_agent(&effective_agent).prepare_host_settings();
        let mut new_ev = new_creds.env_vars.clone();
        for name in &passthrough_names {
            if new_ev.iter().any(|(k, _)| k == name) { continue; }
            if let Ok(val) = std::env::var(name) { new_ev.push((name.clone(), val)); }
        }
        (new_ev, new_hs)
    } else {
        (env_vars, host_settings)
    };

    // Resolve directory overlays from config + env + flags.
    // Malformed --overlay values are fatal (per spec).
    let resolved_overlays = crate::overlays::resolve_overlays(&git_root, raw_overlay_flags)
        .context("invalid --overlay flag")?;
    if !resolved_overlays.is_empty() {
        match final_host_settings.as_mut() {
            Some(hs) => hs.set_overlays(resolved_overlays),
            None => final_host_settings = Some(crate::runtime::HostSettings::overlays_only(resolved_overlays)),
        }
    }

    run_with_sink(
        &OutputSink::Stdout,
        Some(mount_path),
        final_env_vars,
        non_interactive,
        plan,
        final_host_settings.as_ref(),
        allow_docker,
        mount_ssh,
        yolo,
        auto,
        Some(effective_agent),
        model_override.as_deref(),
        &*runtime,
    )
    .await
}

/// Core logic shared between command mode and TUI mode.
///
/// `mount_override`: when `Some`, skip the interactive stdin prompt and use this path.
/// `env_vars`: agent credential env vars to pass into the container.
/// `non_interactive`: when true, launch agent in print/non-interactive mode.
/// `plan`: when true, launch agent in plan (read-only) mode.
/// `allow_docker`: when true, mount the host Docker daemon socket into the container.
/// `mount_ssh`: when true, mount the host `~/.ssh` directory read-only into the container.
/// `yolo`: when true, append `--dangerously-skip-permissions` and disallowed-tools config.
/// `auto`: when true, append `--permission-mode auto` and disallowed-tools config.
/// `agent_override`: when `Some`, use this agent instead of the config value.
/// `model`: when `Some`, pass the model-selection flag to the agent.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_sink(
    out: &OutputSink,
    mount_override: Option<PathBuf>,
    env_vars: Vec<(String, String)>,
    non_interactive: bool,
    plan: bool,
    host_settings: Option<&HostSettings>,
    allow_docker: bool,
    mount_ssh: bool,
    yolo: bool,
    auto: bool,
    agent_override: Option<String>,
    model: Option<&str>,
    runtime: &dyn crate::runtime::AgentRuntime,
) -> Result<()> {
    let git_root = find_git_root().context("Not inside a Git repository")?;
    let config = load_repo_config(&git_root)?;
    let config_agent = config.agent.as_deref().unwrap_or("claude").to_string();
    let agent = agent_override.as_deref().unwrap_or(&config_agent).to_string();

    let mut entrypoint = if non_interactive {
        chat_entrypoint_non_interactive(&agent, plan)
    } else {
        chat_entrypoint(&agent, plan)
    };

    let disallowed_tools = if yolo || auto { effective_yolo_disallowed_tools(&git_root) } else { vec![] };
    append_autonomous_flags(&mut entrypoint, &agent, yolo, auto, &disallowed_tools);

    run_agent_with_sink(
        entrypoint,
        &format!("Starting chat session with agent '{}'", agent),
        out,
        mount_override,
        env_vars,
        non_interactive,
        host_settings,
        allow_docker,
        mount_ssh,
        None,
        agent_override,
        model,
        runtime,
        None,
    )
    .await
}


/// Build the entrypoint command for a chat session (interactive, no prompt).
pub fn chat_entrypoint(agent: &str, plan: bool) -> Vec<String> {
    let mut args = match agent {
        "claude" => vec!["claude".to_string()],
        "codex" => vec!["codex".to_string()],
        "opencode" => vec!["opencode".to_string()],
        "maki" => vec!["maki".to_string()],
        "gemini" => vec!["gemini".to_string()],
        "copilot" => vec!["copilot".to_string()],
        "crush" => vec!["crush".to_string()],
        // cline's interactive entry is via the `task` subcommand (bare `cline` may
        // enter a different UI mode depending on version; `cline task` is stable).
        "cline" => vec!["cline".to_string(), "task".to_string()],
        _ => vec![agent.to_string()],
    };
    append_plan_flags(&mut args, agent, plan);
    args
}

/// Build the entrypoint command for a chat session in non-interactive mode.
pub fn chat_entrypoint_non_interactive(agent: &str, plan: bool) -> Vec<String> {
    let mut args = match agent {
        "claude" => vec!["claude".to_string(), "-p".to_string()],
        "codex" => vec!["codex".to_string()],
        "opencode" => vec!["opencode".to_string()],
        "maki" => vec!["maki".to_string(), "--print".to_string()],
        // Gemini supports -p / --prompt for headless/non-interactive output.
        "gemini" => vec!["gemini".to_string(), "-p".to_string()],
        // copilot: -p puts copilot into prompt/non-interactive mode (reads from stdin)
        "copilot" => vec!["copilot".to_string(), "-p".to_string()],
        // crush: `crush run` with no additional args; prompt supplied separately via stdin or args
        "crush" => vec!["crush".to_string(), "run".to_string()],
        // cline: `cline task --json` triggers non-interactive (structured) output mode
        // without implying autonomous operation. `--yolo` is added separately by
        // append_autonomous_flags when the user passes --yolo to amux.
        "cline" => vec!["cline".to_string(), "task".to_string(), "--json".to_string()],
        _ => vec![agent.to_string()],
    };
    append_plan_flags(&mut args, agent, plan);
    args
}

/// Build the entrypoint command for exec prompt: non-interactive with an injected prompt.
pub fn chat_entrypoint_with_prompt(agent: &str, prompt: &str, plan: bool) -> Vec<String> {
    let mut args = match agent {
        "claude" => vec!["claude".to_string(), "-p".to_string(), prompt.to_string()],
        "codex" => vec!["codex".to_string(), prompt.to_string()],
        "opencode" => vec!["opencode".to_string(), prompt.to_string()],
        "maki" => vec!["maki".to_string(), "--print".to_string(), prompt.to_string()],
        "gemini" => vec!["gemini".to_string(), "-p".to_string(), prompt.to_string()],
        // copilot: -p (prompt mode) + -i <prompt> (initial prompt string)
        "copilot" => vec!["copilot".to_string(), "-p".to_string(), "-i".to_string(), prompt.to_string()],
        // crush: `crush run "<prompt>"` — prompt is positional argument
        "crush" => vec!["crush".to_string(), "run".to_string(), prompt.to_string()],
        // cline: `cline task "<prompt>"` — autonomous flags added separately by append_autonomous_flags
        "cline" => vec!["cline".to_string(), "task".to_string(), prompt.to_string()],
        _ => vec![agent.to_string(), prompt.to_string()],
    };
    append_plan_flags(&mut args, agent, plan);
    args
}

/// Append agent-specific plan mode flags to the argument list.
///
/// - Claude: `--permission-mode plan`
/// - Codex: `--approval-mode plan`
/// - Gemini: `--approval-mode=plan`
/// - Copilot: `--plan`
/// - Cline: `--plan` (on the `task` subcommand)
/// - Opencode: no plan mode available (flag is silently ignored)
/// - Maki: no plan mode available (flag is silently ignored)
/// - Crush: no plan mode available (flag is silently ignored)
fn append_plan_flags(args: &mut Vec<String>, agent: &str, plan: bool) {
    if !plan {
        return;
    }
    match agent {
        "claude" => {
            args.push("--permission-mode".to_string());
            args.push("plan".to_string());
        }
        "codex" => {
            args.push("--approval-mode".to_string());
            args.push("plan".to_string());
        }
        "gemini" => {
            args.push("--approval-mode=plan".to_string());
        }
        // copilot: --plan flag starts directly in plan mode
        "copilot" => {
            args.push("--plan".to_string());
        }
        // cline: --plan flag on the task subcommand
        "cline" => {
            args.push("--plan".to_string());
        }
        // Maki has no plan mode.
        "maki" => {}
        // Crush has no dedicated plan/read-only mode; silently skip.
        "crush" => {}
        // Opencode and unknown agents have no plan mode.
        _ => {}
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_entrypoint_claude() {
        let args = chat_entrypoint("claude", false);
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "claude");
    }

    #[test]
    fn chat_entrypoint_codex() {
        let args = chat_entrypoint("codex", false);
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "codex");
    }

    #[test]
    fn chat_entrypoint_opencode() {
        let args = chat_entrypoint("opencode", false);
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "opencode");
    }

    #[test]
    fn chat_entrypoint_unknown_agent() {
        let args = chat_entrypoint("custom", false);
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "custom");
    }

    #[test]
    fn chat_entrypoint_non_interactive_claude() {
        let args = chat_entrypoint_non_interactive("claude", false);
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "claude");
        assert_eq!(args[1], "-p");
    }

    #[test]
    fn chat_entrypoint_non_interactive_codex() {
        let args = chat_entrypoint_non_interactive("codex", false);
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "codex");
    }

    #[test]
    fn chat_entrypoint_non_interactive_opencode() {
        let args = chat_entrypoint_non_interactive("opencode", false);
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "opencode");
    }

    #[test]
    fn chat_entrypoint_has_no_prompt() {
        for agent in &["claude", "codex", "opencode"] {
            let args = chat_entrypoint(agent, false);
            // Chat should have no prompt argument — just the agent command.
            for arg in &args {
                assert!(
                    !arg.contains("Implement"),
                    "Chat entrypoint for {} should not contain a prompt, found: {}",
                    agent,
                    arg
                );
            }
        }
    }

    #[test]
    fn chat_entrypoint_non_interactive_has_no_prompt() {
        for agent in &["claude", "codex", "opencode"] {
            let args = chat_entrypoint_non_interactive(agent, false);
            for arg in &args {
                assert!(
                    !arg.contains("Implement"),
                    "Chat non-interactive entrypoint for {} should not contain a prompt, found: {}",
                    agent,
                    arg
                );
            }
        }
    }

    // --- Plan mode tests ---

    #[test]
    fn chat_entrypoint_plan_claude() {
        let args = chat_entrypoint("claude", true);
        assert_eq!(args, vec!["claude", "--permission-mode", "plan"]);
    }

    #[test]
    fn chat_entrypoint_plan_codex() {
        let args = chat_entrypoint("codex", true);
        assert_eq!(args, vec!["codex", "--approval-mode", "plan"]);
    }

    #[test]
    fn chat_entrypoint_plan_opencode() {
        // Opencode has no plan mode; flag is silently ignored.
        let args = chat_entrypoint("opencode", true);
        assert_eq!(args, vec!["opencode"]);
    }

    #[test]
    fn chat_entrypoint_plan_unknown_agent() {
        // Unknown agents have no plan mode; flag is silently ignored.
        let args = chat_entrypoint("custom", true);
        assert_eq!(args, vec!["custom"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_plan_claude() {
        let args = chat_entrypoint_non_interactive("claude", true);
        assert_eq!(args, vec!["claude", "-p", "--permission-mode", "plan"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_plan_codex() {
        let args = chat_entrypoint_non_interactive("codex", true);
        assert_eq!(args, vec!["codex", "--approval-mode", "plan"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_plan_opencode() {
        let args = chat_entrypoint_non_interactive("opencode", true);
        assert_eq!(args, vec!["opencode"]);
    }

    // --- maki entrypoints ---

    #[test]
    fn chat_entrypoint_maki() {
        let args = chat_entrypoint("maki", false);
        assert_eq!(args, vec!["maki"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_maki() {
        let args = chat_entrypoint_non_interactive("maki", false);
        assert_eq!(args, vec!["maki", "--print"]);
    }

    #[test]
    fn chat_entrypoint_plan_maki() {
        // Maki has no plan mode; the flag is silently ignored.
        let args = chat_entrypoint("maki", true);
        assert_eq!(args, vec!["maki"]);
    }

    // --- gemini entrypoints ---

    #[test]
    fn chat_entrypoint_gemini() {
        let args = chat_entrypoint("gemini", false);
        assert_eq!(args, vec!["gemini"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_gemini() {
        let args = chat_entrypoint_non_interactive("gemini", false);
        assert_eq!(args, vec!["gemini", "-p"]);
    }

    #[test]
    fn chat_entrypoint_plan_gemini() {
        let args = chat_entrypoint("gemini", true);
        assert_eq!(args, vec!["gemini", "--approval-mode=plan"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_plan_gemini() {
        let args = chat_entrypoint_non_interactive("gemini", true);
        assert_eq!(args, vec!["gemini", "-p", "--approval-mode=plan"]);
    }

    // --- passthrough injection tests ---

    #[test]
    fn passthrough_injection_adds_set_env_var_to_env_vars() {
        use crate::config::{save_repo_config, RepoConfig};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: Some(vec!["AMUX_TEST_PT_INJECT_PRESENT".to_string()]),
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();

        // SAFETY: test-only env mutation; unique var name avoids races with other tests.
        unsafe { std::env::set_var("AMUX_TEST_PT_INJECT_PRESENT", "injected_value_99") };

        // Simulate the passthrough injection loop from chat::run.
        let mut env_vars: Vec<(String, String)> = vec![];
        let passthrough_names = effective_env_passthrough(tmp.path());
        for name in &passthrough_names {
            if let Ok(val) = std::env::var(name) {
                env_vars.push((name.clone(), val));
            }
        }

        // SAFETY: test-only env mutation.
        unsafe { std::env::remove_var("AMUX_TEST_PT_INJECT_PRESENT") };

        assert!(
            env_vars.contains(&("AMUX_TEST_PT_INJECT_PRESENT".to_string(), "injected_value_99".to_string())),
            "set env var must appear in env_vars after passthrough injection"
        );
    }

    #[test]
    fn passthrough_injection_skips_absent_env_var() {
        use crate::config::{save_repo_config, RepoConfig};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        // Use a var name that is very unlikely to be set in any test environment.
        let absent_var = "AMUX_TEST_PT_INJECT_DEFINITELY_NOT_SET_XYZ_999";
        std::env::remove_var(absent_var);

        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: Some(vec![absent_var.to_string()]),
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();

        // Simulate the passthrough injection loop from chat::run.
        let mut env_vars: Vec<(String, String)> = vec![];
        let passthrough_names = effective_env_passthrough(tmp.path());
        for name in &passthrough_names {
            if let Ok(val) = std::env::var(name) {
                env_vars.push((name.clone(), val));
            }
        }

        assert!(
            env_vars.is_empty(),
            "absent env var must not be added to env_vars; no error or panic should occur"
        );
    }

    #[test]
    fn passthrough_injection_skips_var_already_in_credentials() {
        use crate::config::{save_repo_config, RepoConfig};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let var_name = "AMUX_TEST_PT_DEDUP_VAR_UNIQUE_456";

        let config = RepoConfig {
            agent: None,
            auto_agent_auth_accepted: None,
            terminal_scrollback_lines: None,
            yolo_disallowed_tools: None,
            env_passthrough: Some(vec![var_name.to_string()]),
            work_items: None,
            overlays: None,
            agent_stuck_timeout_secs: None,
        };
        save_repo_config(tmp.path(), &config).unwrap();
        // SAFETY: test-only env mutation; unique var name avoids races with other tests.
        unsafe { std::env::set_var(var_name, "passthrough_value") };

        // Simulate starting with the var already present (e.g., from keychain credentials).
        let mut env_vars: Vec<(String, String)> = vec![(var_name.to_string(), "cred_value".to_string())];

        // Simulate the passthrough injection loop from chat::run (with skip-if-present guard).
        let passthrough_names = effective_env_passthrough(tmp.path());
        for name in &passthrough_names {
            if env_vars.iter().any(|(k, _)| k == name) {
                continue; // keychain takes precedence
            }
            if let Ok(val) = std::env::var(name) {
                env_vars.push((name.clone(), val));
            }
        }

        // SAFETY: test-only env mutation.
        unsafe { std::env::remove_var(var_name) };

        // Keychain credential must be present with its original value.
        let entry = env_vars.iter().find(|(k, _)| k == var_name);
        assert!(entry.is_some(), "credential var must remain in env_vars");
        assert_eq!(entry.unwrap().1, "cred_value", "keychain value must not be overwritten by passthrough");

        // Var must appear exactly once — passthrough entry was skipped.
        let count = env_vars.iter().filter(|(k, _)| k == var_name).count();
        assert_eq!(count, 1, "keychain takes precedence: no duplicate -e flag");
    }

    // ── Integration — chat with --model (work item 0055) ─────────────────────
    //
    // run_with_sink() passes the model argument to run_agent_with_sink(), which
    // calls append_model_flag().  These tests verify the full entrypoint
    // construction pipeline: build base entrypoint → append model flag.

    /// `chat --model <name>` in non-interactive mode produces an entrypoint that
    /// includes `--model <name>` after the base args.
    #[test]
    fn chat_non_interactive_with_model_includes_model_flag() {
        use crate::commands::agent::append_model_flag;
        let mut entrypoint = chat_entrypoint_non_interactive("claude", false);
        // Mirror the guard and call in run_agent_with_sink.
        let model: Option<&str> = Some("claude-opus-4-6");
        if let Some(m) = model {
            append_model_flag(&mut entrypoint, "claude", m);
        }
        assert!(
            entrypoint.contains(&"--model".to_string()),
            "--model must appear in the constructed entrypoint"
        );
        assert!(
            entrypoint.contains(&"claude-opus-4-6".to_string()),
            "model name must appear in the constructed entrypoint"
        );
    }

    // --- copilot entrypoints ---

    #[test]
    fn chat_entrypoint_copilot() {
        let args = chat_entrypoint("copilot", false);
        assert_eq!(args, vec!["copilot"]);
    }

    #[test]
    fn chat_entrypoint_copilot_plan() {
        let args = chat_entrypoint("copilot", true);
        assert_eq!(args, vec!["copilot", "--plan"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_copilot() {
        let args = chat_entrypoint_non_interactive("copilot", false);
        assert_eq!(args, vec!["copilot", "-p"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_copilot_plan() {
        let args = chat_entrypoint_non_interactive("copilot", true);
        assert_eq!(args, vec!["copilot", "-p", "--plan"]);
    }

    #[test]
    fn chat_entrypoint_with_prompt_copilot() {
        let args = chat_entrypoint_with_prompt("copilot", "fix bug", false);
        assert_eq!(args, vec!["copilot", "-p", "-i", "fix bug"]);
    }

    #[test]
    fn chat_entrypoint_with_prompt_copilot_plan() {
        let args = chat_entrypoint_with_prompt("copilot", "fix bug", true);
        assert_eq!(args, vec!["copilot", "-p", "-i", "fix bug", "--plan"]);
    }

    // --- crush entrypoints ---

    #[test]
    fn chat_entrypoint_crush() {
        let args = chat_entrypoint("crush", false);
        assert_eq!(args, vec!["crush"]);
    }

    #[test]
    fn chat_entrypoint_crush_plan_silently_skipped() {
        // Crush has no plan mode; the flag is silently ignored.
        let args = chat_entrypoint("crush", true);
        assert_eq!(args, vec!["crush"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_crush() {
        let args = chat_entrypoint_non_interactive("crush", false);
        assert_eq!(args, vec!["crush", "run"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_crush_plan_skipped() {
        // Crush has no plan mode; --plan flag is silently ignored.
        let args = chat_entrypoint_non_interactive("crush", true);
        assert_eq!(args, vec!["crush", "run"]);
    }

    #[test]
    fn chat_entrypoint_with_prompt_crush() {
        let args = chat_entrypoint_with_prompt("crush", "fix bug", false);
        assert_eq!(args, vec!["crush", "run", "fix bug"]);
    }

    #[test]
    fn chat_entrypoint_with_prompt_crush_plan_skipped() {
        // Crush has no plan mode; --plan flag is silently ignored.
        let args = chat_entrypoint_with_prompt("crush", "fix bug", true);
        assert_eq!(args, vec!["crush", "run", "fix bug"]);
    }

    // --- cline entrypoints ---

    #[test]
    fn chat_entrypoint_cline() {
        let args = chat_entrypoint("cline", false);
        assert_eq!(args, vec!["cline", "task"]);
    }

    #[test]
    fn chat_entrypoint_cline_plan() {
        let args = chat_entrypoint("cline", true);
        assert_eq!(args, vec!["cline", "task", "--plan"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_cline() {
        let args = chat_entrypoint_non_interactive("cline", false);
        assert_eq!(args, vec!["cline", "task", "--json"]);
    }

    #[test]
    fn chat_entrypoint_non_interactive_cline_plan() {
        let args = chat_entrypoint_non_interactive("cline", true);
        assert_eq!(args, vec!["cline", "task", "--json", "--plan"]);
    }

    #[test]
    fn chat_entrypoint_with_prompt_cline() {
        // No --yolo for explicit-prompt path; autonomous flags appended separately.
        let args = chat_entrypoint_with_prompt("cline", "fix bug", false);
        assert_eq!(args, vec!["cline", "task", "fix bug"]);
    }

    #[test]
    fn chat_entrypoint_with_prompt_cline_plan() {
        let args = chat_entrypoint_with_prompt("cline", "fix bug", true);
        assert_eq!(args, vec!["cline", "task", "fix bug", "--plan"]);
    }

    /// When no `--model` is given, the entrypoint contains no `--model` flag.
    #[test]
    fn chat_non_interactive_without_model_has_no_model_flag() {
        use crate::commands::agent::append_model_flag;
        let mut entrypoint = chat_entrypoint_non_interactive("claude", false);
        let model: Option<&str> = None;
        if let Some(m) = model {
            append_model_flag(&mut entrypoint, "claude", m);
        }
        assert!(
            !entrypoint.contains(&"--model".to_string()),
            "--model must not appear when model is None"
        );
    }
}
