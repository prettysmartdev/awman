use crate::commands::agent::{append_autonomous_flags, ensure_agent_available, prepare_agent_cli, run_agent_with_sink};
use crate::commands::auth::resolve_auth;
use crate::commands::init_flow::find_git_root;
use crate::commands::output::OutputSink;
use crate::config::{effective_env_passthrough, effective_yolo_disallowed_tools, load_repo_config};
use crate::runtime::{generate_container_name, HostSettings};
use crate::workflow::{self, StepStatus, WorkflowState};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parse a work item string like "0001" or "1" into a u32.
pub fn parse_work_item(s: &str) -> Result<u32> {
    s.parse::<u32>()
        .with_context(|| format!("Invalid work item number: '{}'. Expected a number like 0001.", s))
}

/// Command-mode entry point.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    work_item_str: &str,
    non_interactive: bool,
    plan: bool,
    allow_docker: bool,
    workflow_path: Option<&Path>,
    mut worktree: bool,
    mount_ssh: bool,
    yolo: bool,
    auto: bool,
    agent_override: Option<String>,
    model_override: Option<String>,
    raw_overlay_flags: &[String],
    runtime: std::sync::Arc<dyn crate::runtime::AgentRuntime>,
) -> Result<()> {
    let work_item = parse_work_item(work_item_str)?;
    let git_root = find_git_root().context("Not inside a Git repository")?;

    // --yolo/--auto + --workflow implies --worktree.
    if yolo && workflow_path.is_some() && !worktree {
        println!("--yolo with --workflow implies --worktree. Running in isolated worktree.");
        worktree = true;
    }
    if auto && workflow_path.is_some() && !worktree {
        println!("--auto with --workflow implies --worktree. Running in isolated worktree.");
        worktree = true;
    }

    // Worktree pre-checks.
    if worktree {
        crate::git::git_version_check()?;
        if crate::git::is_detached_head(&git_root) {
            eprintln!(
                "WARNING: You are in detached HEAD state. The worktree branch will be created \
                 from the current commit. Consider checking out a branch first."
            );
        }
    }

    let (mount_path, worktree_branch) = if worktree {
        let wt_path = crate::git::worktree_path(&git_root, work_item)?;
        let branch = crate::git::worktree_branch_name(work_item);

        // Before creating a new worktree, check for uncommitted files on the main branch.
        if !wt_path.exists() {
            let files = crate::git::uncommitted_files(&git_root).unwrap_or_default();
            if !files.is_empty() {
                use std::io::{BufRead, Write};
                eprintln!("WARNING: The current branch has uncommitted changes:");
                for f in &files {
                    eprintln!("  {}", f);
                }
                eprintln!("\nThe worktree will be created from the latest commit.");
                eprintln!("Uncommitted files will NOT be included in the worktree.\n");
                print!("[c]ommit files  [u]se last commit  [a]bort: ");
                std::io::stdout().flush()?;
                let stdin = std::io::stdin();
                let mut lines = stdin.lock().lines();
                let answer = lines.next().unwrap_or(Ok(String::new()))?;
                match answer.trim().to_lowercase().as_str() {
                    "c" | "commit" => {
                        print!("Commit message: ");
                        std::io::stdout().flush()?;
                        let msg = lines.next().unwrap_or(Ok(String::new()))?;
                        let msg = msg.trim().to_string();
                        if msg.is_empty() {
                            anyhow::bail!("Commit message cannot be empty.");
                        }
                        crate::git::commit_all(&git_root, &msg)?;
                        println!("Changes committed.");
                    }
                    "u" | "use" => {
                        println!("Proceeding with last commit (uncommitted changes will not be in the worktree).");
                    }
                    _ => {
                        anyhow::bail!("Aborting: uncommitted changes on current branch.");
                    }
                }
            }
        }

        let wt_path = prepare_worktree_cmd(&git_root, &wt_path, &branch)?;
        (wt_path, Some(branch))
    } else {
        (confirm_mount_scope_stdin(&git_root)?, None)
    };

    let config = load_repo_config(&git_root)?;
    let config_agent = config.agent.as_deref().unwrap_or("claude").to_string();
    let agent = agent_override.as_deref().unwrap_or(&config_agent).to_string();
    let credentials = resolve_auth(&git_root, &agent)?;
    let mut host_settings = crate::passthrough::passthrough_for_agent(&agent).prepare_host_settings();

    // Suppress the dangerous-mode permission dialog when running with --yolo.
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
        // Skip vars already supplied by keychain credentials — keychain takes precedence.
        if env_vars.iter().any(|(k, _)| k == name) {
            continue;
        }
        if let Ok(val) = std::env::var(name) {
            env_vars.push((name.clone(), val));
        }
    }

    if let Some(wf_path) = workflow_path {
        // Resolve relative paths against the process's working directory so that
        // paths like ./aspec/workflows/implement-feature.md work as expected.
        let resolved_wf: PathBuf = if wf_path.is_absolute() {
            wf_path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_else(|_| git_root.clone()).join(wf_path)
        };
        let result = run_workflow(
            Some(work_item),
            &resolved_wf,
            &git_root,
            mount_path.clone(),
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
        .await;
        if let Some(ref branch) = worktree_branch {
            let _ = post_run_merge_prompt_stdin(&git_root, &mount_path, branch);
        }
        return result;
    }

    // Ensure the requested agent is available; offer fallback to default if setup is declined.
    let effective_agent =
        prepare_agent_cli(&git_root, &agent, &config_agent, &*runtime).await?;

    // Recompute credentials and env_vars if fallback changed the agent.
    // Preserve overlays across agent fallback — they are agent-independent.
    let (final_env_vars, final_host_settings) = if effective_agent != agent {
        let new_creds = crate::commands::auth::resolve_auth(&git_root, &effective_agent)?;
        let mut new_hs = crate::passthrough::passthrough_for_agent(&effective_agent).prepare_host_settings();
        // Carry overlays from the original host_settings to the new one.
        let overlays = host_settings.as_ref().map(|hs| hs.overlays.clone()).unwrap_or_default();
        if !overlays.is_empty() {
            match new_hs.as_mut() {
                Some(hs) => hs.set_overlays(overlays),
                None => new_hs = Some(crate::runtime::HostSettings::overlays_only(overlays)),
            }
        }
        let mut new_ev = new_creds.env_vars.clone();
        for name in &passthrough_names {
            if new_ev.iter().any(|(k, _)| k == name) { continue; }
            if let Ok(val) = std::env::var(name) { new_ev.push((name.clone(), val)); }
        }
        (new_ev, new_hs)
    } else {
        (env_vars, host_settings)
    };

    let mut entrypoint = if non_interactive {
        agent_entrypoint_non_interactive(&effective_agent, work_item, plan)
    } else {
        agent_entrypoint(&effective_agent, work_item, plan)
    };

    let disallowed_tools = if yolo || auto { effective_yolo_disallowed_tools(&git_root) } else { vec![] };
    append_autonomous_flags(&mut entrypoint, &effective_agent, yolo, auto, &disallowed_tools);

    let work_item_path = find_work_item(&git_root, work_item)?;
    let status = format!(
        "Implementing work item {:04} with agent '{}': {}",
        work_item,
        &effective_agent,
        work_item_path.display()
    );

    let result = run_agent_with_sink(
        entrypoint,
        &status,
        &OutputSink::Stdout,
        Some(mount_path.clone()),
        final_env_vars,
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
    .await;

    if let Some(ref branch) = worktree_branch {
        let _ = post_run_merge_prompt_stdin(&git_root, &mount_path, branch);
    }

    result
}

/// Core logic shared between command mode and TUI mode.
///
/// `mount_override`: when `Some`, skip the interactive stdin prompt and use this path.
///                   when `None`, prompt via stdin (command mode only).
/// `env_vars`: agent credential env vars to pass into the container.
/// `non_interactive`: when true, launch agent in print/non-interactive mode.
/// `plan`: when true, launch agent in plan (read-only) mode.
/// `allow_docker`: when true, mount the host Docker daemon socket into the container.
/// `worktree`: when true, the worktree has already been set up; `mount_override` is the worktree path.
/// `mount_ssh`: when true, mount the host `~/.ssh` directory read-only into the container.
/// `yolo`: when true, append `--dangerously-skip-permissions` and disallowed-tools config.
/// `auto`: when true, append `--permission-mode auto` and disallowed-tools config.
/// `agent_override`: when `Some`, use this agent instead of the config value.
/// `model`: when `Some`, pass the model-selection flag to the agent.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_sink(
    work_item: u32,
    out: &OutputSink,
    mount_override: Option<PathBuf>,
    env_vars: Vec<(String, String)>,
    non_interactive: bool,
    plan: bool,
    host_settings: Option<&HostSettings>,
    allow_docker: bool,
    worktree: bool,
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
    let work_item_path = find_work_item(&git_root, work_item)?;

    let mut entrypoint = if non_interactive {
        agent_entrypoint_non_interactive(&agent, work_item, plan)
    } else {
        agent_entrypoint(&agent, work_item, plan)
    };

    let disallowed_tools = if yolo || auto { effective_yolo_disallowed_tools(&git_root) } else { vec![] };
    append_autonomous_flags(&mut entrypoint, &agent, yolo, auto, &disallowed_tools);

    let status = format!(
        "Implementing work item {:04} with agent '{}': {}",
        work_item,
        agent,
        work_item_path.display()
    );

    // `worktree` is handled by the TUI directly (launch_implement creates the worktree
    // and sets mount_override before calling run_with_sink). The flag is accepted here
    // for signature consistency but no extra action is needed.
    let _ = worktree;

    run_agent_with_sink(
        entrypoint,
        &status,
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


/// Finds the work item file for the given number, e.g. `aspec/work-items/0001-*.md`.
pub fn find_work_item(git_root: &PathBuf, work_item: u32) -> Result<PathBuf> {
    let pattern = format!("{:04}-", work_item);
    let repo_config = load_repo_config(git_root).unwrap_or_default();
    let (dir_opt, _) = crate::commands::new::resolve_work_item_paths(git_root, &repo_config);

    let dir = dir_opt.ok_or_else(|| {
        anyhow::anyhow!(
            "`implement` requires a work items directory. \
             Run `amux config set work_items.dir <path>` to configure one, \
             or run `amux init --aspec` to set up the aspec folder."
        )
    })?;

    if !dir.exists() {
        bail!("Work items directory not found: {}", dir.display());
    }

    let entry = std::fs::read_dir(&dir)
        .with_context(|| format!("Cannot read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().starts_with(&pattern));

    match entry {
        Some(e) => Ok(e.path()),
        None => bail!("No work item {:04} found in {}", work_item, dir.display()),
    }
}

/// Asks the user (via stdin) whether to mount just CWD or the full Git root.
pub fn confirm_mount_scope_stdin(git_root: &PathBuf) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    if cwd == *git_root {
        return Ok(git_root.clone());
    }

    println!(
        "Mount scope: current directory is '{}', Git root is '{}'.",
        cwd.display(),
        git_root.display()
    );
    print!("Mount the Git root (r) or current directory only (c)? [r/c]: ");

    use std::io::{BufRead, Write};
    std::io::stdout().flush()?;
    let stdin = std::io::stdin();
    let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;

    match answer.trim().to_lowercase().as_str() {
        "r" => Ok(git_root.clone()),
        _ => Ok(cwd),
    }
}

/// The prompt given to the code agent when implementing a work item.
const IMPLEMENT_PROMPT_TEMPLATE: &str = "Implement work item {work_item}. Iterate until the build \
    succeeds. Implement tests as described in the work item and the project aspec. Iterate until \
    tests are comprehensive and pass. Write documentation as described in the project aspec. \
    Ensure final build and test success.";

/// Build the prompt string for the given work item number.
pub fn implement_prompt(work_item: u32) -> String {
    IMPLEMENT_PROMPT_TEMPLATE.replace("{work_item}", &format!("{:04}", work_item))
}

pub fn agent_entrypoint(agent: &str, work_item: u32, plan: bool) -> Vec<String> {
    let prompt = implement_prompt(work_item);

    let mut args = match agent {
        "claude" => vec![
            "claude".to_string(),
            prompt,
        ],
        "codex" => vec![
            "codex".to_string(),
            prompt,
        ],
        "opencode" => vec![
            "opencode".to_string(),
            "run".to_string(),
            prompt,
        ],
        "maki" => vec![
            "maki".to_string(),
            prompt,
        ],
        "gemini" => vec![
            "gemini".to_string(),
            prompt,
        ],
        // copilot: -i starts an interactive session with the initial prompt.
        "copilot" => vec![
            "copilot".to_string(),
            "-i".to_string(),
            prompt,
        ],
        // crush: `crush run "<prompt>"` — prompt as positional arg; run is always non-interactive.
        "crush" => vec![
            "crush".to_string(),
            "run".to_string(),
            prompt,
        ],
        // cline: `cline task "<prompt>"` — interactive task with the work-item prompt.
        "cline" => vec![
            "cline".to_string(),
            "task".to_string(),
            prompt,
        ],
        _ => vec![
            agent.to_string(),
            prompt,
        ],
    };
    append_plan_flags(&mut args, agent, plan);
    args
}

/// Build the entrypoint command for the implement agent in non-interactive (print) mode.
pub fn agent_entrypoint_non_interactive(agent: &str, work_item: u32, plan: bool) -> Vec<String> {
    let prompt = implement_prompt(work_item);

    let mut args = match agent {
        "claude" => vec![
            "claude".to_string(),
            "-p".to_string(),
            prompt,
        ],
        "codex" => vec![
            "codex".to_string(),
            "exec".to_string(),
            prompt,
        ],
        "opencode" => vec![
            "opencode".to_string(),
            "run".to_string(),
            prompt,
        ],
        "maki" => vec![
            "maki".to_string(),
            "--print".to_string(),
            prompt,
        ],
        "gemini" => vec![
            "gemini".to_string(),
            "-p".to_string(),
            prompt,
        ],
        // copilot: -p (prompt/non-interactive mode) + -i <prompt> (initial prompt string).
        "copilot" => vec![
            "copilot".to_string(),
            "-p".to_string(),
            "-i".to_string(),
            prompt,
        ],
        // crush: `crush run "<prompt>"` — run is inherently non-interactive; no extra flag needed.
        "crush" => vec![
            "crush".to_string(),
            "run".to_string(),
            prompt,
        ],
        // cline: `cline task --json "<prompt>"` — --json triggers structured/non-interactive output.
        "cline" => vec![
            "cline".to_string(),
            "task".to_string(),
            "--json".to_string(),
            prompt,
        ],
        _ => vec![
            agent.to_string(),
            prompt,
        ],
    };
    append_plan_flags(&mut args, agent, plan);
    args
}

/// Build an agent entrypoint for a workflow step using a custom prompt.
pub fn workflow_step_entrypoint(agent: &str, prompt: &str, non_interactive: bool, plan: bool) -> Vec<String> {
    let mut args = match (agent, non_interactive) {
        ("claude", true) => vec!["claude".to_string(), "-p".to_string(), prompt.to_string()],
        ("claude", false) => vec!["claude".to_string(), prompt.to_string()],
        ("codex", true) => vec!["codex".to_string(), "exec".to_string(), prompt.to_string()],
        ("codex", false) => vec!["codex".to_string(), prompt.to_string()],
        ("opencode", _) => vec!["opencode".to_string(), "run".to_string(), prompt.to_string()],
        ("maki", true) => vec!["maki".to_string(), "--print".to_string(), prompt.to_string()],
        ("maki", false) => vec!["maki".to_string(), prompt.to_string()],
        ("gemini", true) => vec!["gemini".to_string(), "-p".to_string(), prompt.to_string()],
        ("gemini", false) => vec!["gemini".to_string(), prompt.to_string()],
        // copilot: -p (prompt/non-interactive mode) + -i <prompt> for non-interactive;
        //          -i <prompt> only for interactive (user can continue conversation in PTY).
        ("copilot", true) => vec!["copilot".to_string(), "-p".to_string(), "-i".to_string(), prompt.to_string()],
        ("copilot", false) => vec!["copilot".to_string(), "-i".to_string(), prompt.to_string()],
        // crush: `crush run "<prompt>"` for both modes — run is always prompt-driven and non-interactive.
        ("crush", _) => vec!["crush".to_string(), "run".to_string(), prompt.to_string()],
        // cline: `cline task --json "<prompt>"` for non-interactive (--json triggers structured output);
        //        `cline task "<prompt>"` for interactive (cline detects TTY presence automatically).
        ("cline", true) => vec!["cline".to_string(), "task".to_string(), "--json".to_string(), prompt.to_string()],
        ("cline", false) => vec!["cline".to_string(), "task".to_string(), prompt.to_string()],
        (a, _) => vec![a.to_string(), prompt.to_string()],
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
        // copilot: --plan flag starts directly in plan mode.
        "copilot" => {
            args.push("--plan".to_string());
        }
        // cline: --plan flag on the task subcommand enables read-only planning mode.
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


// ─── Workflow command-mode runner ────────────────────────────────────────────

/// Run a multi-step workflow in command mode (with stdin prompts between steps).
///
/// Steps are executed sequentially in the order they become ready (topological order).
/// After each step the user is prompted to advance or abort.
/// State is persisted to JSON so the workflow can be resumed after an interruption.
#[allow(clippy::too_many_arguments)]
pub async fn run_workflow(
    work_item: Option<u32>,
    workflow_path: &Path,
    git_root: &Path,
    mount_path: PathBuf,
    env_vars: Vec<(String, String)>,
    agent: &str,
    host_settings: Option<HostSettings>,
    non_interactive: bool,
    plan: bool,
    allow_docker: bool,
    mount_ssh: bool,
    yolo: bool,
    auto: bool,
    cli_model: Option<&str>,
    runtime: &dyn crate::runtime::AgentRuntime,
) -> Result<()> {
    use std::io::{BufRead, Write};

    // Load and validate the workflow file.
    let (hash, title, steps) = workflow::load_workflow_file(workflow_path)?;

    let workflow_name = workflow_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("workflow")
        .to_string();

    // Check for an existing state file.
    let state_path = workflow::workflow_state_path(git_root, work_item, &workflow_name);

    let mut state = if state_path.exists() {
        let existing = workflow::load_workflow_state(&state_path)?;
        resolve_resume_or_restart(existing, &hash, &steps, work_item, &workflow_name, &state_path, &agent)?
    } else {
        WorkflowState::new(title.clone(), steps.clone(), hash.clone(), work_item, workflow_name.clone())
    };

    // Persist initial state.
    workflow::save_workflow_state(git_root, &state)?;

    let title_display = state
        .title
        .clone()
        .unwrap_or_else(|| "Workflow".to_string());
    println!("\nRunning workflow: {}", title_display);
    if let Some(wi) = work_item {
        println!("Work item: {:04}", wi);
    }
    println!("Steps: {}", state.steps.len());

    // Load work item content for prompt substitution (empty when no work item).
    let work_item_content = if let Some(wi) = work_item {
        let work_item_path = find_work_item(&PathBuf::from(git_root), wi)?;
        std::fs::read_to_string(&work_item_path)
            .with_context(|| format!("Cannot read work item: {}", work_item_path.display()))?
    } else {
        String::new()
    };

    // ── Pre-flight: validate all required agents ──────────────────────────────
    // Collect the distinct effective agent names required across all steps.
    let mut required_agents: std::collections::HashSet<String> = std::collections::HashSet::new();
    for step in &state.steps {
        let effective = step.agent.as_deref().unwrap_or(agent);
        required_agents.insert(effective.to_string());
    }

    // For each required agent, ensure it is available (Dockerfile + image).
    // Track agents the user declined to set up so we can offer fallback.
    let mut declined_agents: std::collections::HashSet<String> = std::collections::HashSet::new();
    for agent_name in &required_agents {
        let available = ensure_agent_available(
            git_root,
            agent_name,
            &OutputSink::Stdout,
            runtime,
            |name| {
                use std::io::{BufRead, Write};
                print!(
                    "Workflow step requires agent '{}', but its Dockerfile is missing. Download and build the agent image? [y/N]: ",
                    name
                );
                std::io::stdout().flush()?;
                let stdin = std::io::stdin();
                let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
                Ok(answer.trim().eq_ignore_ascii_case("y"))
            },
        )
        .await;
        match available {
            Ok(false) => {
                declined_agents.insert(agent_name.clone());
            }
            Err(e) => {
                eprintln!("Warning: could not set up agent '{}': {}", agent_name, e);
                declined_agents.insert(agent_name.clone());
            }
            Ok(true) => {}
        }
    }

    // For each declined agent, ask once whether to fall back to the default.
    // Build a map: declined_agent → resolved_fallback_agent.
    let mut fallback_map: HashMap<String, String> = HashMap::new();
    {
        use std::io::{BufRead, Write};
        // Collect unique declined agents that differ from the default.
        let mut unique_declined: Vec<String> = declined_agents.iter().cloned().collect();
        unique_declined.sort();
        for declined in &unique_declined {
            if declined.as_str() == agent {
                // Default agent itself was declined — abort.
                bail!(
                    "Aborting workflow: the default agent '{}' is not available.",
                    agent
                );
            }
            print!(
                "Use the default agent ('{}') for steps that specify '{}'? [y/N]: ",
                agent, declined
            );
            std::io::stdout().flush()?;
            let stdin = std::io::stdin();
            let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
            if answer.trim().eq_ignore_ascii_case("y") {
                fallback_map.insert(declined.clone(), agent.to_string());
            } else {
                bail!(
                    "Aborting workflow: agent '{}' is not available and no fallback was accepted.",
                    declined
                );
            }
        }
    }

    // Build the per-step agent map: step_name → effective agent name.
    let mut step_agent_map: HashMap<String, String> = HashMap::new();
    for step in &state.steps {
        let desired = step.agent.as_deref().unwrap_or(agent);
        let effective = if let Some(fallback) = fallback_map.get(desired) {
            fallback.clone()
        } else {
            desired.to_string()
        };
        step_agent_map.insert(step.name.clone(), effective);
    }

    // Handle any previously Running steps (from an interrupted run).
    let interrupted = state.interrupted_running_steps();
    for step_name in interrupted {
        println!("\nStep '{}' was running when the previous session ended.", step_name);
        print!("Start it over (s) or skip to next step (n)? [s/n]: ");
        std::io::stdout().flush()?;
        let stdin = std::io::stdin();
        let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
        if answer.trim().eq_ignore_ascii_case("n") {
            state.set_status(&step_name, StepStatus::Done);
        } else {
            state.set_status(&step_name, StepStatus::Pending);
        }
        workflow::save_workflow_state(git_root, &state)?;
    }

    // Main workflow loop.
    loop {
        let ready = state.next_ready();

        if ready.is_empty() {
            if state.all_done() {
                println!("\nAll workflow steps completed successfully.");
                let _ = std::fs::remove_file(&state_path);
                break;
            } else {
                // Some steps errored — nothing left to do automatically.
                println!("\nNo steps are ready to run. Check for errors above.");
                break;
            }
        }

        // Execute the first ready step (sequential execution).
        let step_name = ready[0].clone();
        let step_state = state
            .get_step(&step_name)
            .expect("ready step exists in state")
            .clone();

        println!("\n─── Step: {} ───", step_name);

        // Resolve the effective agent for this step.
        let step_agent = step_agent_map
            .get(&step_name)
            .map(String::as_str)
            .unwrap_or(agent);

        // Substitute template variables in the prompt.
        let prompt = workflow::substitute_prompt(
            &step_state.prompt_template,
            work_item,
            &work_item_content,
        );

        let mut entrypoint =
            workflow_step_entrypoint(step_agent, &prompt, non_interactive, plan);
        let disallowed_tools = if yolo || auto { effective_yolo_disallowed_tools(git_root) } else { vec![] };
        append_autonomous_flags(&mut entrypoint, step_agent, yolo, auto, &disallowed_tools);
        let status_msg = if let Some(wi) = work_item {
            format!(
                "Workflow step '{}' — work item {:04} with agent '{}'",
                step_name, wi, step_agent
            )
        } else {
            format!(
                "Workflow step '{}' with agent '{}'",
                step_name, step_agent
            )
        };

        // Resolve model: step-level Model: field takes precedence over CLI --model.
        let step_model: Option<&str> = step_state.model.as_deref().or(cli_model);

        // Generate a container name and record it for state persistence.
        let container_name = generate_container_name();
        state.set_container_id(&step_name, container_name.clone());

        // Mark step as Running and save state.
        state.set_status(&step_name, StepStatus::Running);
        workflow::save_workflow_state(git_root, &state)?;

        let result = run_agent_with_sink(
            entrypoint,
            &status_msg,
            &OutputSink::Stdout,
            Some(mount_path.clone()),
            env_vars.clone(),
            non_interactive,
            host_settings.as_ref(),
            allow_docker,
            mount_ssh,
            Some(container_name),
            Some(step_agent.to_string()),
            step_model,
            runtime,
            None,
        )
        .await;

        match result {
            Ok(_) => {
                state.set_status(&step_name, StepStatus::Done);
                workflow::save_workflow_state(git_root, &state)?;

                if state.all_done() {
                    println!("\nStep '{}' completed. Workflow finished!", step_name);
                    let _ = std::fs::remove_file(&state_path);
                    break;
                }

                println!("\nStep '{}' completed.", step_name);
                let next = state.next_ready();
                if !next.is_empty() {
                    println!("Next step(s): {}", next.join(", "));
                }
                print!("Press [Enter] to advance, or [q] to abort: ");
                std::io::stdout().flush()?;
                let stdin = std::io::stdin();
                let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
                if answer.trim().eq_ignore_ascii_case("q") {
                    println!("Workflow paused. Run again to resume.");
                    break;
                }
            }
            Err(e) => {
                state.set_status(&step_name, StepStatus::Error(e.to_string()));
                workflow::save_workflow_state(git_root, &state)?;

                println!("\nStep '{}' failed: {}", step_name, e);
                print!("Press [r] to retry, or any other key to abort: ");
                std::io::stdout().flush()?;
                let stdin = std::io::stdin();
                let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
                if answer.trim().eq_ignore_ascii_case("r") {
                    state.set_status(&step_name, StepStatus::Pending);
                    workflow::save_workflow_state(git_root, &state)?;
                    // Continue loop — the step will appear ready again.
                } else {
                    println!("Workflow paused. Run again to resume from the failed step.");
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Resolve whether to resume an existing workflow state or start fresh.
///
/// Handles hash mismatch detection and interrupted-run step recovery.
/// `default_agent` is the CLI's current effective agent (from --agent flag or config).
/// A warning is printed when resuming if any persisted step specifies an agent that
/// differs from `default_agent`, so the user knows per-step overrides are in effect.
fn resolve_resume_or_restart(
    existing: WorkflowState,
    new_hash: &str,
    new_steps: &[workflow::parser::WorkflowStep],
    work_item: Option<u32>,
    workflow_name: &str,
    state_path: &Path,
    default_agent: &str,
) -> Result<WorkflowState> {
    use std::io::{BufRead, Write};

    if let Some(wi) = work_item {
        println!(
            "\nFound a saved workflow state for '{}' (work item {:04}).",
            workflow_name, wi
        );
    } else {
        println!(
            "\nFound a saved workflow state for '{}'.",
            workflow_name
        );
    }

    if existing.workflow_hash != new_hash {
        println!("WARNING: The workflow file has changed since the last run.");
        print!("  1) Restart from the beginning\n  2) Continue anyway (could be dangerous)\n  [1/2]: ");
        std::io::stdout().flush()?;
        let stdin = std::io::stdin();
        let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;

        if answer.trim() == "2" {
            // Attempt to resume — validate step structure compatibility.
            match workflow::validate_resume_compatibility(&existing, new_steps) {
                Ok(_) => {
                    println!("Resuming with changed workflow file.");
                    return Ok(existing);
                }
                Err(e) => {
                    println!("Cannot resume: {}", e);
                    println!("Restarting from the beginning.");
                    // Fall through to restart.
                }
            }
        }

        // Restart: delete old state file, create fresh.
        let _ = std::fs::remove_file(state_path);
        return Ok(WorkflowState::new(
            existing.title,
            new_steps.to_vec(),
            new_hash.to_string(),
            work_item,
            workflow_name.to_string(),
        ));
    }

    // Hash matches — offer resume or restart.
    print!("  1) Resume from where you left off\n  2) Restart from the beginning\n  [1/2]: ");
    std::io::stdout().flush()?;
    let stdin = std::io::stdin();
    let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;

    if answer.trim() == "2" {
        let _ = std::fs::remove_file(state_path);
        return Ok(WorkflowState::new(
            existing.title,
            new_steps.to_vec(),
            new_hash.to_string(),
            work_item,
            workflow_name.to_string(),
        ));
    }

    println!("Resuming previous workflow run.");
    warn_resume_agent_overrides(&existing, default_agent);
    Ok(existing)
}

/// Print a warning when resuming if any persisted step specifies a different agent
/// than the current CLI default. This alerts the user that per-step `Agent:` overrides
/// will take precedence over the `--agent` flag for those steps.
fn warn_resume_agent_overrides(state: &WorkflowState, default_agent: &str) {
    let overridden: Vec<(&str, &str)> = state
        .steps
        .iter()
        .filter(|s| {
            s.agent
                .as_deref()
                .map(|a| a != default_agent)
                .unwrap_or(false)
        })
        .map(|s| (s.name.as_str(), s.agent.as_deref().unwrap()))
        .collect();

    if overridden.is_empty() {
        return;
    }

    println!(
        "Note: the following steps specify an agent that differs from the current default ('{}'):",
        default_agent
    );
    for (name, agent) in &overridden {
        println!("  step '{}' → agent '{}'", name, agent);
    }
    println!("Per-step agent overrides take precedence over the --agent flag.");
}

// ─── Worktree helpers (command mode) ─────────────────────────────────────────

/// Prepare (or reuse) a worktree at `wt_path` on `branch` using stdin prompts.
///
/// If the worktree directory already exists the user is prompted to resume or
/// recreate it.  Otherwise the worktree is created fresh.
pub fn prepare_worktree_cmd(git_root: &Path, wt_path: &PathBuf, branch: &str) -> Result<PathBuf> {
    use std::io::{BufRead, Write};
    if wt_path.exists() {
        println!("Worktree already exists at {}.", wt_path.display());
        print!("[r]esume / [R]ecreate? ");
        std::io::stdout().flush()?;
        let stdin = std::io::stdin();
        let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
        if answer.trim() == "R" {
            crate::git::remove_worktree(git_root, wt_path)?;
            crate::git::create_worktree(git_root, wt_path, branch)?;
        }
        // 'r' or any other key: reuse existing worktree
    } else {
        crate::git::create_worktree(git_root, wt_path, branch)?;
    }
    Ok(wt_path.clone())
}

/// After the container (or workflow) completes, ask the user whether to merge,
/// discard, or keep the worktree branch.
fn post_run_merge_prompt_stdin(git_root: &Path, wt_path: &Path, branch: &str) -> Result<()> {
    use std::io::{BufRead, Write};
    println!(
        "\nWorktree branch `{}` is ready. Merge into current branch? [y/n/s(kip-and-keep)]",
        branch
    );
    print!("> ");
    std::io::stdout().flush()?;
    let stdin = std::io::stdin();
    let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
    match answer.trim().to_lowercase().as_str() {
        "y" | "yes" | "m" | "merge" => match crate::git::merge_branch(git_root, branch) {
            Ok(()) => {
                let _ = crate::git::remove_worktree(git_root, wt_path);
                let _ = crate::git::delete_branch(git_root, branch);
                println!("Merged and cleaned up worktree.");
            }
            Err(e) => {
                eprintln!("Merge failed with conflicts: {}", e);
                eprintln!(
                    "Resolve manually in `{}`, then run:\n  git branch -d {} && git worktree remove {}",
                    git_root.display(),
                    branch,
                    wt_path.display()
                );
            }
        },
        "n" | "no" | "d" | "discard" => {
            let _ = crate::git::remove_worktree(git_root, wt_path);
            let _ = crate::git::delete_branch(git_root, branch);
            println!("Worktree discarded.");
        }
        _ => {
            // 's', 'skip', or any other input: skip and keep
            println!("Worktree kept at: {}", wt_path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_work_item(dir: &PathBuf, name: &str) {
        std::fs::create_dir_all(dir.join("aspec/work-items")).unwrap();
        std::fs::write(dir.join("aspec/work-items").join(name), "# Work Item").unwrap();
    }

    #[test]
    fn find_work_item_matches_by_prefix() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        make_work_item(&root, "0001-add-feature.md");
        let path = find_work_item(&root, 1).unwrap();
        assert!(path.ends_with("0001-add-feature.md"));
    }

    #[test]
    fn find_work_item_errors_when_missing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("aspec/work-items")).unwrap();
        assert!(find_work_item(&root, 99).is_err());
    }

    #[test]
    fn agent_entrypoint_claude() {
        let args = agent_entrypoint("claude", 1, false);
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "claude");
        assert!(args[1].contains("work item 0001"));
    }

    #[test]
    fn agent_entrypoint_codex() {
        let args = agent_entrypoint("codex", 2, false);
        assert_eq!(args[0], "codex");
        assert!(args[1].contains("work item 0002"));
    }

    #[test]
    fn agent_entrypoint_opencode() {
        let args = agent_entrypoint("opencode", 3, false);
        assert_eq!(args[0], "opencode");
        assert_eq!(args[1], "run");
        assert!(args[2].contains("work item 0003"));
    }

    #[test]
    fn implement_prompt_includes_work_item_number() {
        let prompt = implement_prompt(42);
        assert!(prompt.contains("work item 0042"));
        assert!(prompt.contains("Iterate until the build succeeds"));
        assert!(prompt.contains("Ensure final build and test success"));
    }

    #[test]
    fn parse_work_item_valid_inputs() {
        assert_eq!(parse_work_item("1").unwrap(), 1);
        assert_eq!(parse_work_item("0001").unwrap(), 1);
        assert_eq!(parse_work_item("42").unwrap(), 42);
        assert_eq!(parse_work_item("0042").unwrap(), 42);
    }

    #[test]
    fn parse_work_item_invalid_inputs() {
        assert!(parse_work_item("abc").is_err());
        assert!(parse_work_item("").is_err());
        assert!(parse_work_item("-1").is_err());
    }

    #[test]
    fn agent_entrypoint_non_interactive_claude() {
        let args = agent_entrypoint_non_interactive("claude", 1, false);
        assert_eq!(args[0], "claude");
        assert_eq!(args[1], "-p");
        assert!(args[2].contains("work item 0001"));
    }

    #[test]
    fn agent_entrypoint_non_interactive_codex() {
        let args = agent_entrypoint_non_interactive("codex", 2, false);
        assert_eq!(args[0], "codex");
        assert_eq!(args[1], "exec");
        assert!(args[2].contains("work item 0002"));
    }

    #[test]
    fn agent_entrypoint_non_interactive_opencode() {
        let args = agent_entrypoint_non_interactive("opencode", 3, false);
        assert_eq!(args[0], "opencode");
        assert_eq!(args[1], "run");
        assert!(args[2].contains("work item 0003"));
    }

    #[test]
    fn agent_entrypoint_gemini() {
        let args = agent_entrypoint("gemini", 4, false);
        assert_eq!(args[0], "gemini");
        assert!(args[1].contains("work item 0004"));
    }

    #[test]
    fn agent_entrypoint_non_interactive_gemini() {
        let args = agent_entrypoint_non_interactive("gemini", 4, false);
        assert_eq!(args[0], "gemini");
        assert_eq!(args[1], "-p");
        assert!(args[2].contains("work item 0004"));
    }

    #[test]
    fn agent_entrypoint_plan_gemini() {
        let args = agent_entrypoint("gemini", 4, true);
        assert_eq!(args[0], "gemini");
        assert!(args[1].contains("work item 0004"));
        assert_eq!(args[2], "--approval-mode=plan");
    }

    #[test]
    fn agent_entrypoint_non_interactive_plan_gemini() {
        let args = agent_entrypoint_non_interactive("gemini", 4, true);
        assert_eq!(args[0], "gemini");
        assert_eq!(args[1], "-p");
        assert!(args[2].contains("work item 0004"));
        assert_eq!(args[3], "--approval-mode=plan");
    }

    #[test]
    fn workflow_step_entrypoint_gemini_interactive() {
        let args = workflow_step_entrypoint("gemini", "my prompt", false, false);
        assert_eq!(args[0], "gemini");
        assert_eq!(args[1], "my prompt");
    }

    #[test]
    fn workflow_step_entrypoint_gemini_non_interactive() {
        let args = workflow_step_entrypoint("gemini", "my prompt", true, false);
        assert_eq!(args[0], "gemini");
        assert_eq!(args[1], "-p");
        assert_eq!(args[2], "my prompt");
    }

    // --- copilot entrypoints ---

    #[test]
    fn agent_entrypoint_copilot() {
        let args = agent_entrypoint("copilot", 1, false);
        assert_eq!(args[0], "copilot");
        assert_eq!(args[1], "-i");
        assert!(args[2].contains("work item 0001"));
    }

    #[test]
    fn agent_entrypoint_copilot_plan() {
        // --plan is appended after the prompt for copilot.
        let args = agent_entrypoint("copilot", 1, true);
        assert_eq!(args[0], "copilot");
        assert_eq!(args[1], "-i");
        assert!(args[2].contains("work item 0001"));
        assert_eq!(args[3], "--plan");
    }

    #[test]
    fn agent_entrypoint_non_interactive_copilot() {
        let args = agent_entrypoint_non_interactive("copilot", 1, false);
        assert_eq!(args[0], "copilot");
        assert_eq!(args[1], "-p");
        assert_eq!(args[2], "-i");
        assert!(args[3].contains("work item 0001"));
    }

    #[test]
    fn agent_entrypoint_non_interactive_copilot_plan() {
        let args = agent_entrypoint_non_interactive("copilot", 1, true);
        assert_eq!(args[0], "copilot");
        assert_eq!(args[1], "-p");
        assert_eq!(args[2], "-i");
        assert!(args[3].contains("work item 0001"));
        assert_eq!(args[4], "--plan");
    }

    #[test]
    fn workflow_step_entrypoint_copilot_non_interactive() {
        let args = workflow_step_entrypoint("copilot", "step prompt", true, false);
        assert_eq!(args, vec!["copilot", "-p", "-i", "step prompt"]);
    }

    #[test]
    fn workflow_step_entrypoint_copilot_interactive() {
        let args = workflow_step_entrypoint("copilot", "step prompt", false, false);
        assert_eq!(args, vec!["copilot", "-i", "step prompt"]);
    }

    // --- crush entrypoints ---

    #[test]
    fn agent_entrypoint_crush() {
        let args = agent_entrypoint("crush", 1, false);
        assert_eq!(args[0], "crush");
        assert_eq!(args[1], "run");
        assert!(args[2].contains("work item 0001"));
    }

    #[test]
    fn agent_entrypoint_crush_plan_skipped() {
        // Crush has no plan mode; flag is silently ignored.
        let args = agent_entrypoint("crush", 1, true);
        assert_eq!(args[0], "crush");
        assert_eq!(args[1], "run");
        assert!(args[2].contains("work item 0001"));
        assert_eq!(args.len(), 3, "no --plan flag must be appended for crush");
    }

    #[test]
    fn agent_entrypoint_non_interactive_crush() {
        // crush run is inherently non-interactive; no extra flag needed.
        let args = agent_entrypoint_non_interactive("crush", 1, false);
        assert_eq!(args[0], "crush");
        assert_eq!(args[1], "run");
        assert!(args[2].contains("work item 0001"));
    }

    #[test]
    fn agent_entrypoint_non_interactive_crush_plan_skipped() {
        // Crush has no plan mode; flag is silently ignored in non-interactive mode too.
        let args = agent_entrypoint_non_interactive("crush", 1, true);
        assert_eq!(args[0], "crush");
        assert_eq!(args[1], "run");
        assert!(args[2].contains("work item 0001"));
        assert_eq!(args.len(), 3, "no --plan flag must be appended for crush");
    }

    #[test]
    fn workflow_step_entrypoint_crush_non_interactive() {
        // crush run is always prompt-driven and non-interactive; same for both modes.
        let args = workflow_step_entrypoint("crush", "step prompt", true, false);
        assert_eq!(args, vec!["crush", "run", "step prompt"]);
    }

    #[test]
    fn workflow_step_entrypoint_crush_interactive() {
        // crush run is always prompt-driven; interactive mode produces same vector.
        let args = workflow_step_entrypoint("crush", "step prompt", false, false);
        assert_eq!(args, vec!["crush", "run", "step prompt"]);
    }

    // --- cline entrypoints ---

    #[test]
    fn agent_entrypoint_cline() {
        let args = agent_entrypoint("cline", 1, false);
        assert_eq!(args[0], "cline");
        assert_eq!(args[1], "task");
        assert!(args[2].contains("work item 0001"));
    }

    #[test]
    fn agent_entrypoint_cline_plan() {
        let args = agent_entrypoint("cline", 1, true);
        assert_eq!(args[0], "cline");
        assert_eq!(args[1], "task");
        assert!(args[2].contains("work item 0001"));
        assert_eq!(args[3], "--plan");
    }

    #[test]
    fn agent_entrypoint_non_interactive_cline() {
        // --json triggers structured/non-interactive output mode.
        let args = agent_entrypoint_non_interactive("cline", 1, false);
        assert_eq!(args[0], "cline");
        assert_eq!(args[1], "task");
        assert_eq!(args[2], "--json");
        assert!(args[3].contains("work item 0001"));
    }

    #[test]
    fn agent_entrypoint_non_interactive_cline_plan() {
        let args = agent_entrypoint_non_interactive("cline", 1, true);
        assert_eq!(args[0], "cline");
        assert_eq!(args[1], "task");
        assert_eq!(args[2], "--json");
        assert!(args[3].contains("work item 0001"));
        assert_eq!(args[4], "--plan");
    }

    #[test]
    fn workflow_step_entrypoint_cline_non_interactive() {
        // --json triggers structured output; non-interactive for headless use.
        let args = workflow_step_entrypoint("cline", "step prompt", true, false);
        assert_eq!(args, vec!["cline", "task", "--json", "step prompt"]);
    }

    #[test]
    fn workflow_step_entrypoint_cline_interactive() {
        // Interactive: cline detects TTY presence; no --json needed.
        let args = workflow_step_entrypoint("cline", "step prompt", false, false);
        assert_eq!(args, vec!["cline", "task", "step prompt"]);
    }

    // --- append_plan_flags for new agents (regression guards) ---

    #[test]
    fn append_plan_flags_copilot_appends_plan() {
        let mut args = vec!["copilot".to_string(), "prompt".to_string()];
        append_plan_flags(&mut args, "copilot", true);
        assert!(args.contains(&"--plan".to_string()), "copilot must receive --plan");
    }

    #[test]
    fn append_plan_flags_crush_skipped() {
        // Crush has no plan mode; args must be unchanged.
        let mut args = vec!["crush".to_string(), "run".to_string(), "prompt".to_string()];
        let original_len = args.len();
        append_plan_flags(&mut args, "crush", true);
        assert_eq!(args.len(), original_len, "no flag must be appended for crush");
    }

    #[test]
    fn append_plan_flags_cline_appends_plan() {
        let mut args = vec!["cline".to_string(), "task".to_string(), "prompt".to_string()];
        append_plan_flags(&mut args, "cline", true);
        assert!(args.contains(&"--plan".to_string()), "cline must receive --plan");
    }

    #[test]
    fn append_plan_flags_maki_no_plan_regression() {
        // Maki has no plan mode; regression guard to ensure it remains unchanged.
        let mut args = vec!["maki".to_string(), "prompt".to_string()];
        let original_len = args.len();
        append_plan_flags(&mut args, "maki", true);
        assert_eq!(args.len(), original_len, "no flag must be appended for maki");
    }

    // --- Plan mode tests ---

    #[test]
    fn agent_entrypoint_plan_claude() {
        let args = agent_entrypoint("claude", 1, true);
        assert_eq!(args[0], "claude");
        assert!(args[1].contains("work item 0001"));
        assert_eq!(args[2], "--permission-mode");
        assert_eq!(args[3], "plan");
    }

    #[test]
    fn agent_entrypoint_plan_codex() {
        let args = agent_entrypoint("codex", 2, true);
        assert_eq!(args[0], "codex");
        assert!(args[1].contains("work item 0002"));
        assert_eq!(args[2], "--approval-mode");
        assert_eq!(args[3], "plan");
    }

    #[test]
    fn agent_entrypoint_plan_opencode() {
        // Opencode has no plan mode; flag is silently ignored.
        let args = agent_entrypoint("opencode", 3, true);
        assert_eq!(args.len(), 3); // opencode, run, prompt — no extra flags
        assert_eq!(args[0], "opencode");
        assert_eq!(args[1], "run");
    }

    #[test]
    fn agent_entrypoint_plan_unknown_agent() {
        let args = agent_entrypoint("custom", 1, true);
        assert_eq!(args.len(), 2); // agent, prompt — no extra flags
    }

    #[test]
    fn agent_entrypoint_non_interactive_plan_claude() {
        let args = agent_entrypoint_non_interactive("claude", 1, true);
        assert_eq!(args[0], "claude");
        assert_eq!(args[1], "-p");
        assert!(args[2].contains("work item 0001"));
        assert_eq!(args[3], "--permission-mode");
        assert_eq!(args[4], "plan");
    }

    #[test]
    fn agent_entrypoint_non_interactive_plan_codex() {
        let args = agent_entrypoint_non_interactive("codex", 2, true);
        assert_eq!(args[0], "codex");
        assert_eq!(args[1], "exec");
        assert!(args[2].contains("work item 0002"));
        assert_eq!(args[3], "--approval-mode");
        assert_eq!(args[4], "plan");
    }

    #[test]
    fn agent_entrypoint_non_interactive_plan_opencode() {
        let args = agent_entrypoint_non_interactive("opencode", 3, true);
        assert_eq!(args.len(), 3); // opencode, run, prompt — no extra flags
    }

    // --- Workflow step entrypoint tests ---

    #[test]
    fn workflow_step_entrypoint_claude_interactive() {
        let args = workflow_step_entrypoint("claude", "my prompt", false, false);
        assert_eq!(args[0], "claude");
        assert_eq!(args[1], "my prompt");
    }

    #[test]
    fn workflow_step_entrypoint_claude_non_interactive() {
        let args = workflow_step_entrypoint("claude", "my prompt", true, false);
        assert_eq!(args[0], "claude");
        assert_eq!(args[1], "-p");
        assert_eq!(args[2], "my prompt");
    }

    #[test]
    fn workflow_step_entrypoint_codex_non_interactive() {
        let args = workflow_step_entrypoint("codex", "prompt", true, false);
        assert_eq!(args[0], "codex");
        assert_eq!(args[1], "exec");
        assert_eq!(args[2], "prompt");
    }

    #[test]
    fn workflow_step_entrypoint_with_plan() {
        let args = workflow_step_entrypoint("claude", "prompt", false, true);
        assert!(args.contains(&"--permission-mode".to_string()));
        assert!(args.contains(&"plan".to_string()));
    }

    // --- Worktree implication tests ---
    // The implication logic is embedded in run(); we mirror the exact condition
    // here to test all branches without spinning up a real git repo.

    fn apply_worktree_implication(
        yolo: bool,
        auto: bool,
        workflow: Option<&str>,
        worktree: bool,
    ) -> (bool, bool) {
        let mut wt = worktree;
        let mut message_printed = false;
        if yolo && workflow.is_some() && !wt {
            message_printed = true;
            wt = true;
        }
        if auto && workflow.is_some() && !wt {
            message_printed = true;
            wt = true;
        }
        (wt, message_printed)
    }

    #[test]
    fn worktree_implied_when_yolo_and_workflow_without_worktree() {
        let (wt, msg) = apply_worktree_implication(true, false, Some("steps.md"), false);
        assert!(wt, "worktree must be set to true when yolo + workflow");
        assert!(msg, "implication message must be printed");
    }

    #[test]
    fn worktree_implied_when_auto_and_workflow_without_worktree() {
        let (wt, msg) = apply_worktree_implication(false, true, Some("steps.md"), false);
        assert!(wt, "worktree must be set to true when auto + workflow");
        assert!(msg, "implication message must be printed");
    }

    #[test]
    fn worktree_not_implied_when_yolo_without_workflow() {
        let (wt, msg) = apply_worktree_implication(true, false, None, false);
        assert!(!wt, "worktree must NOT be implied without --workflow");
        assert!(!msg, "message must not be printed");
    }

    #[test]
    fn worktree_not_implied_when_auto_without_workflow() {
        let (wt, msg) = apply_worktree_implication(false, true, None, false);
        assert!(!wt, "worktree must NOT be implied without --workflow");
        assert!(!msg, "message must not be printed");
    }

    #[test]
    fn worktree_implication_idempotent_when_already_set() {
        // --yolo --worktree --workflow: worktree stays true, no message printed.
        let (wt, msg) = apply_worktree_implication(true, false, Some("steps.md"), true);
        assert!(wt, "worktree must remain true");
        assert!(!msg, "message must NOT print when --worktree was already passed");
    }

    #[test]
    fn worktree_implication_auto_idempotent_when_already_set() {
        let (wt, msg) = apply_worktree_implication(false, true, Some("steps.md"), true);
        assert!(wt, "worktree must remain true");
        assert!(!msg, "message must NOT print when --worktree was already passed");
    }

    #[test]
    fn worktree_not_implied_when_no_yolo_no_auto() {
        let (wt, msg) = apply_worktree_implication(false, false, Some("steps.md"), false);
        assert!(!wt, "worktree must not be set without --yolo or --auto");
        assert!(!msg);
    }

    // ─── Workflow pre-flight: per-step agent resolution ───────────────────────
    //
    // The full run_workflow() function requires stdin and real file I/O, so we
    // test the pure pre-flight logic that builds the per-step agent map and the
    // fallback map. These mirror the cases documented in work item 0052.

    /// Build a minimal WorkflowState where every step carries its specified agent.
    fn make_state_with_agents(step_agents: &[(&str, Option<&str>)]) -> crate::workflow::WorkflowState {
        let steps: Vec<crate::workflow::parser::WorkflowStep> = step_agents
            .iter()
            .map(|(name, agent)| crate::workflow::parser::WorkflowStep {
                name: name.to_string(),
                depends_on: vec![],
                prompt_template: "p".to_string(),
                agent: agent.map(|a| a.to_string()),
                model: None,
            })
            .collect();
        crate::workflow::WorkflowState::new(None, steps, "hash".into(), Some(1), "wf".into())
    }

    /// Compute the per-step agent map given a state, default agent, and fallback map.
    /// Mirrors the logic inlined in run_workflow().
    fn compute_step_agent_map(
        state: &crate::workflow::WorkflowState,
        default_agent: &str,
        fallback_map: &std::collections::HashMap<String, String>,
    ) -> std::collections::HashMap<String, String> {
        state
            .steps
            .iter()
            .map(|s| {
                let desired = s.agent.as_deref().unwrap_or(default_agent);
                let effective = fallback_map
                    .get(desired)
                    .cloned()
                    .unwrap_or_else(|| desired.to_string());
                (s.name.clone(), effective)
            })
            .collect()
    }

    #[test]
    fn preflight_all_agents_available_uses_per_step_agents() {
        // All steps have explicit agents; no agent is declined.
        let state = make_state_with_agents(&[("plan", Some("codex")), ("impl", Some("claude"))]);
        let fallback: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let map = compute_step_agent_map(&state, "claude", &fallback);

        assert_eq!(map.get("plan").map(String::as_str), Some("codex"));
        assert_eq!(map.get("impl").map(String::as_str), Some("claude"));
    }

    #[test]
    fn preflight_step_without_agent_uses_default() {
        // Steps without an Agent: field fall back to the workflow default.
        let state = make_state_with_agents(&[("plan", None), ("impl", Some("codex"))]);
        let fallback: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let map = compute_step_agent_map(&state, "claude", &fallback);

        assert_eq!(map.get("plan").map(String::as_str), Some("claude"),
            "step without Agent: must use the workflow default agent");
        assert_eq!(map.get("impl").map(String::as_str), Some("codex"));
    }

    #[test]
    fn preflight_declined_agent_replaced_by_fallback() {
        // When the user declines to set up "codex", all steps requesting it
        // must be redirected to the fallback (default) agent.
        let state = make_state_with_agents(&[("plan", Some("codex")), ("impl", None)]);
        let mut fallback = std::collections::HashMap::new();
        // Simulate user accepting the fallback: codex → claude.
        fallback.insert("codex".to_string(), "claude".to_string());
        let map = compute_step_agent_map(&state, "claude", &fallback);

        assert_eq!(
            map.get("plan").map(String::as_str),
            Some("claude"),
            "declined codex must be replaced by the accepted fallback"
        );
        assert_eq!(
            map.get("impl").map(String::as_str),
            Some("claude"),
            "step with no Agent: field must still use the default"
        );
    }

    #[test]
    fn preflight_multiple_steps_different_agents_no_fallback() {
        // Three steps, three different agents, none declined.
        let state = make_state_with_agents(&[
            ("a", Some("claude")),
            ("b", Some("codex")),
            ("c", Some("gemini")),
        ]);
        let fallback: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let map = compute_step_agent_map(&state, "claude", &fallback);

        assert_eq!(map.get("a").map(String::as_str), Some("claude"));
        assert_eq!(map.get("b").map(String::as_str), Some("codex"));
        assert_eq!(map.get("c").map(String::as_str), Some("gemini"));
    }

    // ─── Model resolution in workflow runner (work item 0055) ─────────────────
    //
    // run_workflow() resolves the effective model for each step via:
    //   let step_model = step_state.model.as_deref().or(cli_model);
    // The three paths are: step model wins, CLI fallback, neither yields None.
    // We test the pure resolution logic directly to avoid the stdin/file-I/O
    // complexity of run_workflow itself.

    /// Mirror the single resolution line from run_workflow().
    fn resolve_step_model<'a>(
        step_model: Option<&'a str>,
        cli_model: Option<&'a str>,
    ) -> Option<&'a str> {
        step_model.or(cli_model)
    }

    #[test]
    fn model_resolution_step_model_wins_over_cli_flag() {
        // A per-step Model: field takes precedence over the CLI --model flag.
        let result = resolve_step_model(Some("model-a"), Some("model-b"));
        assert_eq!(
            result,
            Some("model-a"),
            "step-level model must win over the CLI --model flag"
        );
    }

    #[test]
    fn model_resolution_cli_flag_used_when_step_has_none() {
        // When the step has no Model: field, the CLI --model flag is used.
        let result = resolve_step_model(None, Some("model-b"));
        assert_eq!(
            result,
            Some("model-b"),
            "CLI --model must be used when the step has no Model: field"
        );
    }

    #[test]
    fn model_resolution_neither_yields_none() {
        // When neither the step nor the CLI provides a model, the result is None.
        let result = resolve_step_model(None, None);
        assert!(
            result.is_none(),
            "model must be None when neither step nor CLI supplies one"
        );
    }

    // ── Integration — implement with --model (work item 0055) ─────────────────
    //
    // run_with_sink() passes the model argument to run_agent_with_sink(), which
    // calls append_model_flag().  These tests verify the full entrypoint
    // construction pipeline mirrored from run_with_sink().

    /// `implement --model <name>` in non-interactive mode produces an entrypoint
    /// that includes `--model <name>`.
    #[test]
    fn implement_non_interactive_with_model_includes_model_flag() {
        use crate::commands::agent::append_model_flag;
        let mut entrypoint = agent_entrypoint_non_interactive("claude", 42, false);
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

    /// When no `--model` is given, the entrypoint contains no `--model` flag.
    #[test]
    fn implement_non_interactive_without_model_has_no_model_flag() {
        use crate::commands::agent::append_model_flag;
        let mut entrypoint = agent_entrypoint_non_interactive("claude", 42, false);
        let model: Option<&str> = None;
        if let Some(m) = model {
            append_model_flag(&mut entrypoint, "claude", m);
        }
        assert!(
            !entrypoint.contains(&"--model".to_string()),
            "--model must not appear when model is None"
        );
    }

    // ── Integration — workflow with per-step Model: fields (work item 0055) ──
    //
    // The full run_workflow() function requires stdin and real file I/O.
    // These tests verify the pure resolution logic extracted from run_workflow().
    // For each step, the effective model is:
    //   step_state.model.as_deref().or(cli_model)

    fn make_state_with_models(step_models: &[(&str, Option<&str>)]) -> crate::workflow::WorkflowState {
        let steps: Vec<crate::workflow::parser::WorkflowStep> = step_models
            .iter()
            .map(|(name, model)| crate::workflow::parser::WorkflowStep {
                name: name.to_string(),
                depends_on: vec![],
                prompt_template: "p".to_string(),
                agent: None,
                model: model.map(|m| m.to_string()),
            })
            .collect();
        crate::workflow::WorkflowState::new(None, steps, "hash".into(), Some(1), "wf".into())
    }

    /// Workflow: step A has `Model: model-a`, step B has none.
    /// CLI: `--model model-b`.
    /// Expected: step A uses `model-a`, step B uses `model-b`.
    #[test]
    fn workflow_per_step_model_wins_over_cli_flag() {
        let state = make_state_with_models(&[("a", Some("model-a")), ("b", None)]);
        let cli_model: Option<&str> = Some("model-b");

        let model_a = resolve_step_model(
            state.get_step("a").unwrap().model.as_deref(),
            cli_model,
        );
        let model_b = resolve_step_model(
            state.get_step("b").unwrap().model.as_deref(),
            cli_model,
        );

        assert_eq!(model_a, Some("model-a"), "step A model must override the CLI flag");
        assert_eq!(model_b, Some("model-b"), "step B must fall back to the CLI flag");
    }

    /// Workflow: neither step has a `Model:` field and no `--model` flag is given.
    /// Expected: both steps receive `None` (no model flag).
    #[test]
    fn workflow_no_model_fields_and_no_cli_flag_gives_none() {
        let state = make_state_with_models(&[("a", None), ("b", None)]);
        let cli_model: Option<&str> = None;

        let model_a = resolve_step_model(
            state.get_step("a").unwrap().model.as_deref(),
            cli_model,
        );
        let model_b = resolve_step_model(
            state.get_step("b").unwrap().model.as_deref(),
            cli_model,
        );

        assert!(model_a.is_none(), "step A must have no model when neither step nor CLI supplies one");
        assert!(model_b.is_none(), "step B must have no model when neither step nor CLI supplies one");
    }
}
