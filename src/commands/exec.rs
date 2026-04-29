use crate::commands::agent::{append_autonomous_flags, prepare_agent_cli, run_agent_with_sink};
use crate::commands::auth::resolve_auth;
use crate::commands::chat::chat_entrypoint_with_prompt;
use crate::commands::implement::{confirm_mount_scope_stdin, parse_work_item, run_workflow};
use crate::commands::init_flow::find_git_root;
use crate::commands::output::OutputSink;
use crate::config::{effective_env_passthrough, effective_yolo_disallowed_tools, load_repo_config};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Command-mode entry point for `amux exec prompt <prompt>`.
#[allow(clippy::too_many_arguments)]
pub async fn run_prompt(
    prompt: &str,
    non_interactive: bool,
    plan: bool,
    allow_docker: bool,
    mount_ssh: bool,
    yolo: bool,
    auto: bool,
    agent_override: Option<String>,
    model_override: Option<String>,
    raw_overlay_flags: &[String],
    runtime: std::sync::Arc<dyn crate::runtime::AgentRuntime>,
) -> Result<()> {
    let git_root = find_git_root().context("Not inside a Git repository")?;
    let mount_path = confirm_mount_scope_stdin(&git_root)?;
    let config = load_repo_config(&git_root)?;
    let config_agent = config.agent.as_deref().unwrap_or("claude").to_string();
    let agent = agent_override.as_deref().unwrap_or(&config_agent).to_string();
    let credentials = resolve_auth(&git_root, &agent)?;
    let host_settings = crate::passthrough::passthrough_for_agent(&agent).prepare_host_settings();

    if yolo {
        if let Some(ref s) = host_settings {
            let _ = s.apply_yolo_settings();
        }
    }

    let mut env_vars = credentials.env_vars.clone();
    let passthrough_names = effective_env_passthrough(&git_root);
    for name in &passthrough_names {
        if env_vars.iter().any(|(k, _)| k == name) {
            continue;
        }
        if let Ok(val) = std::env::var(name) {
            env_vars.push((name.clone(), val));
        }
    }

    let effective_agent = prepare_agent_cli(&git_root, &agent, &config_agent, &*runtime).await?;

    let (final_env_vars, mut final_host_settings) = if effective_agent != agent {
        let new_creds = resolve_auth(&git_root, &effective_agent)?;
        let new_hs =
            crate::passthrough::passthrough_for_agent(&effective_agent).prepare_host_settings();
        let mut new_ev = new_creds.env_vars.clone();
        for name in &passthrough_names {
            if new_ev.iter().any(|(k, _)| k == name) {
                continue;
            }
            if let Ok(val) = std::env::var(name) {
                new_ev.push((name.clone(), val));
            }
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

    let mut entrypoint = chat_entrypoint_with_prompt(&effective_agent, prompt, plan);
    let disallowed_tools = if yolo || auto {
        effective_yolo_disallowed_tools(&git_root)
    } else {
        vec![]
    };
    append_autonomous_flags(
        &mut entrypoint,
        &effective_agent,
        yolo,
        auto,
        &disallowed_tools,
    );

    let status = format!(
        "Exec prompt with agent '{}': {}",
        effective_agent,
        if prompt.len() > 60 {
            format!("{}…", &prompt[..57])
        } else {
            prompt.to_string()
        }
    );

    run_agent_with_sink(
        entrypoint,
        &status,
        &OutputSink::Stdout,
        Some(mount_path),
        final_env_vars,
        // chat_entrypoint_with_prompt always uses the agent's non-interactive flag
        // (e.g. `claude -p <prompt>`), so the container is always run without a PTY.
        // We still thread non_interactive through so that run_agent_with_sink can
        // apply any non_interactive-specific container settings consistently.
        non_interactive,
        final_host_settings.as_ref(),
        allow_docker,
        mount_ssh,
        None,
        Some(effective_agent),
        model_override.as_deref(),
        &*runtime,
        None,
    )
    .await
}

/// Command-mode entry point for `amux exec workflow <path>`.
#[allow(clippy::too_many_arguments)]
pub async fn run_exec_workflow(
    workflow_path: &Path,
    work_item_str: Option<&str>,
    non_interactive: bool,
    plan: bool,
    allow_docker: bool,
    mut worktree: bool,
    mount_ssh: bool,
    yolo: bool,
    auto: bool,
    agent_override: Option<String>,
    model_override: Option<String>,
    raw_overlay_flags: &[String],
    runtime: std::sync::Arc<dyn crate::runtime::AgentRuntime>,
) -> Result<()> {
    let work_item = work_item_str.map(parse_work_item).transpose()?;
    let git_root = find_git_root().context("Not inside a Git repository")?;

    // --yolo/--auto implies --worktree.
    if yolo && !worktree {
        println!("--yolo implies --worktree. Running in isolated worktree.");
        worktree = true;
    }
    if auto && !worktree {
        println!("--auto implies --worktree. Running in isolated worktree.");
        worktree = true;
    }

    let mount_path = if worktree {
        crate::git::git_version_check()?;
        if crate::git::is_detached_head(&git_root) {
            eprintln!(
                "WARNING: You are in detached HEAD state. The worktree branch will be created \
                 from the current commit."
            );
        }
        // Derive worktree path and branch from the work item when provided, or from the
        // workflow file name otherwise. Using the file stem avoids collisions between
        // different no-work-item workflows and with actual work item 0000.
        let (wt_path, branch) = match work_item {
            Some(wi) => {
                let path = crate::git::worktree_path(&git_root, wi)?;
                let br = crate::git::worktree_branch_name(wi);
                (path, br)
            }
            None => {
                let wf_name = workflow_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("workflow");
                let path = crate::git::worktree_path_named(&git_root, wf_name)?;
                let br = crate::git::worktree_branch_name_for_workflow(wf_name);
                (path, br)
            }
        };
        crate::commands::implement::prepare_worktree_cmd(&git_root, &wt_path, &branch)?
    } else {
        confirm_mount_scope_stdin(&git_root)?
    };

    let config = load_repo_config(&git_root)?;
    let config_agent = config.agent.as_deref().unwrap_or("claude").to_string();
    let agent = agent_override.as_deref().unwrap_or(&config_agent).to_string();
    let credentials = resolve_auth(&git_root, &agent)?;
    let mut host_settings = crate::passthrough::passthrough_for_agent(&agent).prepare_host_settings();

    if yolo {
        if let Some(ref s) = host_settings {
            let _ = s.apply_yolo_settings();
        }
    }

    // Resolve directory overlays from config + env + flags.
    // Malformed --overlay values are fatal (per spec).
    let resolved_overlays = crate::overlays::resolve_overlays(&git_root, raw_overlay_flags)
        .context("invalid --overlay flag")?;
    if !resolved_overlays.is_empty() {
        match host_settings.as_mut() {
            Some(hs) => hs.set_overlays(resolved_overlays),
            None => host_settings = Some(crate::runtime::HostSettings::overlays_only(resolved_overlays)),
        }
    }

    let mut env_vars = credentials.env_vars.clone();
    let passthrough_names = effective_env_passthrough(&git_root);
    for name in &passthrough_names {
        if env_vars.iter().any(|(k, _)| k == name) {
            continue;
        }
        if let Ok(val) = std::env::var(name) {
            env_vars.push((name.clone(), val));
        }
    }

    // Resolve the workflow path relative to the current directory.
    let resolved_wf: PathBuf = if workflow_path.is_absolute() {
        workflow_path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| git_root.clone())
            .join(workflow_path)
    };

    run_workflow(
        work_item,
        &resolved_wf,
        &git_root,
        mount_path,
        env_vars,
        &agent,
        host_settings,
        non_interactive,
        plan,
        allow_docker,
        mount_ssh,
        yolo,
        auto,
        model_override.as_deref(),
        &*runtime,
    )
    .await
}

#[cfg(test)]
mod tests {
    use crate::commands::chat::{chat_entrypoint_non_interactive, chat_entrypoint_with_prompt};
    use crate::commands::implement::parse_work_item;

    // ── prompt entrypoint parity with chat::run_with_sink ────────────────────
    //
    // run_prompt builds its entrypoint via chat_entrypoint_with_prompt, while
    // chat::run_with_sink (non_interactive=true) uses chat_entrypoint_non_interactive.
    // When plan=false the two differ only by the injected prompt at the end.
    // This test verifies that relationship holds for every supported agent.

    #[test]
    fn prompt_entrypoint_equals_non_interactive_plus_prompt_no_plan() {
        const PROMPT: &str = "implement feature X";
        for agent in &["claude", "codex", "opencode", "maki", "gemini"] {
            let ni_base = chat_entrypoint_non_interactive(agent, false);
            let with_prompt = chat_entrypoint_with_prompt(agent, PROMPT, false);
            let mut expected = ni_base.clone();
            expected.push(PROMPT.to_string());
            assert_eq!(
                with_prompt, expected,
                "{}: chat_entrypoint_with_prompt(plan=false) must equal \
                 chat_entrypoint_non_interactive(plan=false) + [prompt]; \
                 got {:?}, expected {:?}",
                agent, with_prompt, expected
            );
        }
    }

    #[test]
    fn prompt_entrypoint_with_plan_contains_both_prompt_and_plan_flags_claude() {
        const PROMPT: &str = "plan this task";
        let args = chat_entrypoint_with_prompt("claude", PROMPT, true);
        assert!(args.contains(&PROMPT.to_string()), "prompt must be present; got: {:?}", args);
        assert!(
            args.contains(&"--permission-mode".to_string()),
            "claude plan flag --permission-mode must be present; got: {:?}",
            args
        );
        assert!(args.contains(&"plan".to_string()), "plan value must be present; got: {:?}", args);
    }

    #[test]
    fn prompt_entrypoint_with_plan_contains_both_prompt_and_plan_flags_codex() {
        const PROMPT: &str = "plan this task";
        let args = chat_entrypoint_with_prompt("codex", PROMPT, true);
        assert!(args.contains(&PROMPT.to_string()), "prompt must be present; got: {:?}", args);
        assert!(
            args.contains(&"--approval-mode".to_string()),
            "codex plan flag --approval-mode must be present; got: {:?}",
            args
        );
    }

    // ── run_workflow: work_item = None skips parse ────────────────────────────
    //
    // run_exec_workflow uses:
    //   let work_item = work_item_str.map(parse_work_item).transpose()?;
    // When work_item_str is None, parse_work_item must NOT be called and the
    // result must be Ok(None) — no work item substitution occurs.

    #[test]
    fn workflow_none_work_item_produces_none_without_error() {
        // Mirrors the expression in run_exec_workflow exactly.
        let result: anyhow::Result<Option<u32>> =
            None::<&str>.map(parse_work_item).transpose();
        assert!(
            result.is_ok(),
            "None work_item_str must not produce an error; got: {:?}",
            result.err()
        );
        assert!(
            result.unwrap().is_none(),
            "None work_item_str must produce None (no work item context)"
        );
    }

    #[test]
    fn workflow_some_valid_work_item_parses_correctly() {
        let result: anyhow::Result<Option<u32>> =
            Some("0053").map(parse_work_item).transpose();
        assert!(result.is_ok(), "valid work item '0053' must parse without error");
        assert_eq!(result.unwrap(), Some(53));
    }

    #[test]
    fn workflow_some_invalid_work_item_returns_error() {
        let result: anyhow::Result<Option<u32>> =
            Some("not-a-number").map(parse_work_item).transpose();
        assert!(result.is_err(), "invalid work item string must produce a parse error");
    }
}
