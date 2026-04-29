use crate::commands::init_flow::find_git_root;
use crate::commands::output::OutputSink;
use crate::config::load_repo_config;
use crate::runtime::{agent_image_tag, project_image_tag, HostSettings};
use anyhow::{Context, Result};
use std::path::PathBuf;
use dirs;
use reqwest;

/// GitHub raw URL template for per-agent Dockerfile downloads.
/// Each entry: (agent_name, dockerfile_url)
static AGENT_DOCKERFILE_URLS: &[(&str, &str)] = &[
    ("claude",    "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates/Dockerfile.claude"),
    ("codex",     "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates/Dockerfile.codex"),
    ("opencode",  "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates/Dockerfile.opencode"),
    ("maki",      "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates/Dockerfile.maki"),
    ("gemini",    "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates/Dockerfile.gemini"),
    ("copilot",   "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates/Dockerfile.copilot"),
    ("crush",     "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates/Dockerfile.crush"),
    ("cline",     "https://raw.githubusercontent.com/prettysmartdev/amux/main/templates/Dockerfile.cline"),
];

/// Resolves which Docker image tag and Dockerfile path to use for a given agent.
///
/// Always returns agent-specific paths. The Dockerfile may not yet exist (e.g. when
/// the agent has not been set up yet); callers must check existence before use.
///
/// Returns `(image_tag, dockerfile_path)`.
pub fn resolve_agent_image_and_dockerfile(
    git_root: &std::path::Path,
    agent_name: &str,
) -> (String, std::path::PathBuf) {
    let agent_dockerfile = git_root.join(".amux").join(format!("Dockerfile.{}", agent_name));
    let agent_tag = agent_image_tag(git_root, agent_name);
    (agent_tag, agent_dockerfile)
}

/// Ensure an agent's Dockerfile and image are available, prompting the user to
/// download and build them if missing.
///
/// Returns:
/// - `Ok(true)`  — the agent is ready (Dockerfile exists or was just created and built).
/// - `Ok(false)` — the user declined, or a download/build error occurred (error printed via `out`).
/// - `Err(_)`    — a programming error (e.g. no URL known for the agent name).
///
/// `ask_fn` is called with the agent name when the Dockerfile is missing. It
/// should return `Ok(true)` if the user wants to download and build, `Ok(false)`
/// to decline.
pub async fn ensure_agent_available<F>(
    git_root: &std::path::Path,
    agent_name: &str,
    out: &OutputSink,
    runtime: &dyn crate::runtime::AgentRuntime,
    ask_fn: F,
) -> Result<bool>
where
    F: FnOnce(&str) -> Result<bool>,
{
    ensure_agent_available_inner(git_root, agent_name, out, runtime, ask_fn, AGENT_DOCKERFILE_URLS).await
}

/// Inner implementation of `ensure_agent_available`, taking an explicit URL map for testability.
async fn ensure_agent_available_inner<F>(
    git_root: &std::path::Path,
    agent_name: &str,
    out: &OutputSink,
    runtime: &dyn crate::runtime::AgentRuntime,
    ask_fn: F,
    url_map: &[(&str, &str)],
) -> Result<bool>
where
    F: FnOnce(&str) -> Result<bool>,
{
    let agent_dockerfile = git_root.join(".amux").join(format!("Dockerfile.{}", agent_name));

    // Dockerfile already exists — agent is available (image may still need building,
    // but that is handled at launch time).
    if agent_dockerfile.exists() {
        return Ok(true);
    }

    // Dockerfile missing — ask the user whether to download and build it.
    if !ask_fn(agent_name)? {
        return Ok(false);
    }

    // Find the download URL for this agent.
    let url = url_map
        .iter()
        .find(|(name, _)| *name == agent_name)
        .map(|(_, url)| *url)
        .ok_or_else(|| anyhow::anyhow!(
            "No Dockerfile template URL known for agent '{}'. \
             Create .amux/Dockerfile.{} manually.",
            agent_name, agent_name
        ))?;

    // Download the Dockerfile template.
    out.println(format!("Downloading Dockerfile.{}…", agent_name));
    let client = reqwest::Client::builder()
        .user_agent("amux")
        .build()
        .context("Failed to build HTTP client")?;
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            out.println(format!("Error: failed to download Dockerfile.{}: {}", agent_name, e));
            return Ok(false);
        }
    };
    if !resp.status().is_success() {
        out.println(format!(
            "Error: failed to download Dockerfile.{}: HTTP {} from {}",
            agent_name, resp.status(), url
        ));
        return Ok(false);
    }
    let content = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            out.println(format!("Error: failed to read Dockerfile.{} response body: {}", agent_name, e));
            return Ok(false);
        }
    };

    // Substitute the {{AMUX_BASE_IMAGE}} placeholder with the project's base image tag
    // so the downloaded Dockerfile builds on top of this project's customised base image.
    let project_base = project_image_tag(git_root);
    let content = content.replace("{{AMUX_BASE_IMAGE}}", &project_base);

    // Save the Dockerfile.
    if let Some(parent) = agent_dockerfile.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create .amux directory: {}", parent.display()))?;
    }
    std::fs::write(&agent_dockerfile, &content)
        .with_context(|| format!("Cannot write {}", agent_dockerfile.display()))?;
    out.println(format!("Saved {}", agent_dockerfile.display()));

    // Build the agent image.
    if !runtime.image_exists(&project_base) {
        anyhow::bail!(
            "Project base image {} is not built. Run `amux ready` first.",
            project_base
        );
    }
    let agent_tag = agent_image_tag(git_root, agent_name);
    out.println(format!("Building {}…", agent_tag));
    let git_root_str = git_root.to_str().unwrap_or(".");
    let out_clone = out.clone();
    let build_result = runtime.build_image_streaming(
        &agent_tag,
        &agent_dockerfile,
        std::path::Path::new(git_root_str),
        false,
        &mut |line| { out_clone.println(line); },
    );
    match build_result {
        Ok(_) => {
            out.println(format!("Agent image {} built successfully.", agent_tag));
            Ok(true)
        }
        Err(e) => {
            out.println(format!("Error: failed to build agent image {}: {}", agent_tag, e));
            // Build failed — remove the Dockerfile so we don't leave a partial state.
            let _ = std::fs::remove_file(&agent_dockerfile);
            Ok(false)
        }
    }
}

/// Build an agent image from an existing Dockerfile.
///
/// Called from the TUI when the user accepts the `AgentSetupConfirm` dialog
/// with `image_only: true` — the Dockerfile is already present but the
/// Docker image has not been built yet.
///
/// Returns:
/// - `Ok(true)`  — the image was built successfully.
/// - `Ok(false)` — the build failed (error printed via `out`).
/// - `Err(_)`    — the project base image is not built.
pub fn build_agent_image(
    git_root: &std::path::Path,
    agent_name: &str,
    out: &OutputSink,
    runtime: &dyn crate::runtime::AgentRuntime,
) -> Result<bool> {
    let agent_dockerfile = git_root.join(".amux").join(format!("Dockerfile.{}", agent_name));
    if !agent_dockerfile.exists() {
        anyhow::bail!(
            "Agent '{}' Dockerfile not found at {}",
            agent_name,
            agent_dockerfile.display()
        );
    }
    let agent_tag = agent_image_tag(git_root, agent_name);
    if runtime.image_exists(&agent_tag) {
        return Ok(true);
    }
    let project_base = project_image_tag(git_root);
    if !runtime.image_exists(&project_base) {
        anyhow::bail!(
            "Project base image {} is not built. Run `amux ready` first.",
            project_base
        );
    }
    out.println(format!("Agent image {} not found. Building from {}…", agent_tag, agent_dockerfile.display()));
    let git_root_str = git_root.to_str().unwrap_or(".");
    let out_clone = out.clone();
    let build_result = runtime.build_image_streaming(
        &agent_tag,
        &agent_dockerfile,
        std::path::Path::new(git_root_str),
        false,
        &mut |line| { out_clone.println(line); },
    );
    match build_result {
        Ok(_) => {
            out.println(format!("Agent image {} built successfully.", agent_tag));
            Ok(true)
        }
        Err(e) => {
            out.println(format!("Error: failed to build agent image {}: {}", agent_tag, e));
            Ok(false)
        }
    }
}

/// CLI mode: ensure the requested agent is available, prompting via stdin.
///
/// If the agent Dockerfile is missing and the user declines to download/build it,
/// offers to fall back to `config_default` instead. Returns the effective agent
/// name to use (may equal `agent` or `config_default`).
pub async fn prepare_agent_cli(
    git_root: &std::path::Path,
    agent: &str,
    config_default: &str,
    runtime: &dyn crate::runtime::AgentRuntime,
) -> Result<String> {
    let available = ensure_agent_available(
        git_root,
        agent,
        &OutputSink::Stdout,
        runtime,
        |name| {
            use std::io::{BufRead, Write};
            print!(
                "Agent '{}' Dockerfile is missing. Download and build the agent image? [y/N]: ",
                name
            );
            std::io::stdout().flush()?;
            let stdin = std::io::stdin();
            let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
            Ok(answer.trim().eq_ignore_ascii_case("y"))
        },
    )
    .await?;

    if available {
        return Ok(agent.to_string());
    }

    // User declined (or setup failed). If the requested agent is already the configured
    // default, there is no fallback — abort.
    if agent == config_default {
        anyhow::bail!(
            "Agent '{}' is not available and no fallback is possible \
             (it is the configured default). Run `amux ready` to build it.",
            agent
        );
    }

    // Offer to fall back to the configured default agent.
    use std::io::{BufRead, Write};
    print!("Use the default agent ('{}') instead? [y/N]: ", config_default);
    std::io::stdout().flush()?;
    let stdin = std::io::stdin();
    let answer = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
    if answer.trim().eq_ignore_ascii_case("y") {
        Ok(config_default.to_string())
    } else {
        anyhow::bail!(
            "Agent '{}' is not available and fallback to '{}' was declined.",
            agent, config_default
        );
    }
}

/// Shared logic for launching a containerized agent session.
///
/// Used by both `implement` (with a pre-configured prompt) and `chat` (no prompt).
///
/// `entrypoint`: the Docker entrypoint command (agent + optional prompt).
/// `status_message`: displayed to the user before launching.
/// `mount_override`: when `Some`, skip the interactive stdin prompt and use this path.
/// `env_vars`: agent credential env vars to pass into the container.
/// `non_interactive`: when true, launch agent in print/non-interactive mode.
/// `allow_docker`: when true, mount the host Docker daemon socket into the container.
/// `mount_ssh`: when true, mount the host `~/.ssh` directory read-only into the container.
/// `agent_override`: when `Some`, use this agent name instead of the config value.
/// `model`: when `Some`, append the per-agent model-selection flag to the entrypoint.
pub async fn run_agent_with_sink(
    mut entrypoint: Vec<String>,
    status_message: &str,
    out: &OutputSink,
    mount_override: Option<PathBuf>,
    env_vars: Vec<(String, String)>,
    non_interactive: bool,
    host_settings: Option<&HostSettings>,
    allow_docker: bool,
    mount_ssh: bool,
    container_name_override: Option<String>,
    agent_override: Option<String>,
    model: Option<&str>,
    runtime: &dyn crate::runtime::AgentRuntime,
    // Callers that already know the git root (e.g. TUI, where the tab CWD may
    // differ from the process CWD) should supply it here to avoid a redundant
    // and potentially wrong `find_git_root()` call.
    git_root_override: Option<PathBuf>,
) -> Result<()> {
    let git_root = match git_root_override {
        Some(gr) => gr,
        None => find_git_root().context("Not inside a Git repository")?,
    };
    let config = load_repo_config(&git_root)?;
    let config_agent = config.agent.as_deref().unwrap_or("claude").to_string();
    let agent = agent_override.as_deref().unwrap_or(&config_agent).to_string();

    // Validate agent name if overridden
    if let Some(ref name) = agent_override {
        crate::cli::validate_agent_name(name)?;
    }

    // Append model-selection flag last (after any autonomous/plan flags already in entrypoint).
    if let Some(m) = model {
        append_model_flag(&mut entrypoint, &agent, m);
    }

    out.println(status_message);

    let mount_path = match mount_override {
        Some(p) => p,
        None => crate::commands::implement::confirm_mount_scope_stdin(&git_root)?,
    };

    // If --allow-docker, check the socket and print a warning before launching.
    if allow_docker {
        let socket_path = runtime.check_socket()
            .context("Cannot mount socket")?;
        out.println(format!("{} socket: {} (found)", runtime.name(), socket_path.display()));
        out.println(format!(
            "WARNING: --allow-docker: mounting host {} socket into container ({}:{}). \
             This grants the agent elevated host access.",
            runtime.name(),
            socket_path.display(),
            socket_path.display()
        ));
    }

    // If --allow-ssh, resolve ~/.ssh, validate it exists, and warn before launching.
    let ssh_dir: Option<PathBuf> = if mount_ssh {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Cannot resolve home directory"))?;
        let ssh = home.join(".ssh");
        if !ssh.exists() {
            anyhow::bail!("Host ~/.ssh directory not found; cannot use --mount-ssh");
        }
        out.println(
            "WARNING: --mount-ssh: mounting host ~/.ssh into container (read-only). \
             SSH keys with incorrect permissions may be rejected by git inside the container — \
             verify host key permissions (e.g. chmod 600 ~/.ssh/id_*). \
             Ensure you trust the agent image."
                .to_string(),
        );
        Some(ssh)
    } else {
        None
    };

    // Determine which image to use and the dockerfile for USER detection.
    let agent_dockerfile = git_root.join(".amux").join(format!("Dockerfile.{}", agent));
    if !agent_dockerfile.exists() {
        anyhow::bail!(
            "Agent '{}' is not set up: .amux/Dockerfile.{} not found. \
             Run `amux ready` to build agent images, or use `--agent <name>` \
             to request a different agent.",
            agent, agent
        );
    }
    let image_tag = agent_image_tag(&git_root, &agent);

    let entrypoint_refs: Vec<&str> = entrypoint.iter().map(String::as_str).collect();

    // Detect the last USER directive in the agent dockerfile
    // and update settings mounts to target the correct home directory inside the container.
    let modified_settings: Option<crate::runtime::HostSettings> = host_settings.and_then(|settings| {
        let mut new_settings = settings.clone_view();
        if let Some(msg) = crate::runtime::apply_dockerfile_user(&mut new_settings, &agent_dockerfile) {
            out.println(msg);
            Some(new_settings)
        } else {
            None
        }
    });
    let effective_settings: Option<&crate::runtime::HostSettings> =
        modified_settings.as_ref().or(host_settings);

    // Show the full runtime CLI command being run (with masked env values).
    let display_args = runtime.build_run_args_display(
        &image_tag,
        mount_path.to_str().unwrap(),
        &entrypoint_refs,
        &env_vars,
        effective_settings,
        allow_docker,
        container_name_override.as_deref(),
        ssh_dir.as_deref(),
    );
    out.println(format!("$ {} {}", runtime.cli_binary(), display_args.join(" ")));

    // Ensure the agent image is available, building it if needed.
    if !runtime.image_exists(&image_tag) {
        // Agent dockerfile exists but image doesn't — build it (first-run case).
        let project_tag = project_image_tag(&git_root);
        if !runtime.image_exists(&project_tag) {
            anyhow::bail!(
                "Agent image {} not found and project base image {} is not built. \
                 Run `amux ready` first to build both images.",
                image_tag, project_tag
            );
        }
        out.println(format!("Agent image {} not found. Building from {}...", image_tag, agent_dockerfile.display()));
        let git_root_str = git_root.to_str().unwrap().to_string();
        let out_clone = out.clone();
        runtime.build_image_streaming(
            &image_tag,
            &agent_dockerfile,
            std::path::Path::new(&git_root_str),
            false,
            &mut |line| { out_clone.println(line); },
        ).context("Failed to build agent image")?;
        out.println(format!("Agent image {} built successfully.", image_tag));
    }

    if !non_interactive {
        crate::commands::ready::print_interactive_notice(out, &agent);
    } else {
        out.println("Tip: remove --non-interactive to interact with the agent directly.");
    }

    if non_interactive {
        let (_cmd, output) = runtime.run_container_captured(
            &image_tag,
            mount_path.to_str().unwrap(),
            &entrypoint_refs,
            &env_vars,
            effective_settings,
            allow_docker,
            container_name_override.as_deref(),
            ssh_dir.as_deref(),
        )
        .context("Container exited with an error")?;
        for line in output.lines() {
            out.println(line);
        }
    } else {
        runtime.run_container(
            &image_tag,
            mount_path.to_str().unwrap(),
            &entrypoint_refs,
            &env_vars,
            effective_settings,
            allow_docker,
            container_name_override.as_deref(),
            ssh_dir.as_deref(),
        )
        .context("Container exited with an error")?;
    }

    Ok(())
}

/// Append agent-specific model-selection flag to the argument list.
///
/// All currently supported agents use `--model <name>` as a direct CLI flag.
/// Per-agent format expectations for `<name>`:
/// - `claude`, `codex`, `gemini`: bare model ID (e.g. `claude-opus-4-6`, `gpt-4o`).
/// - `opencode`: `provider/model` is **required** (e.g. `anthropic/claude-3-5-sonnet`).
/// - `crush`: bare model ID *or* `provider/model` to disambiguate when multiple
///   providers expose models with the same name. Flag goes on the `run` subcommand.
/// - `maki`: `provider/model-id` (e.g. `anthropic/claude-opus-4-6`).
/// - `cline`: bare model ID; the provider is selected separately via `cline auth -p`
///   and is not switchable per-invocation through `--model`.
/// - `copilot`: no CLI flag — model selection is via the `/model` interactive
///   slash command, so `--model` is dropped with a warning.
pub fn append_model_flag(args: &mut Vec<String>, agent: &str, model: &str) {
    match agent {
        "claude" | "codex" | "gemini" | "cline" | "crush" | "opencode" | "maki" => {
            args.push("--model".to_string());
            args.push(model.to_string());
        }
        "copilot" => {
            eprintln!(
                "WARNING: --model: agent 'copilot' does not support --model as a CLI flag \
                 (model selection is via the /model interactive command); proceeding without the flag."
            );
        }
        _ => {
            eprintln!(
                "WARNING: --model: agent '{}' does not support --model; \
                 proceeding without the flag.",
                agent
            );
        }
    }
}

/// Append agent-specific autonomous-mode flags and disallowed-tools config.
///
/// When `yolo` is true:
/// - Claude: `--dangerously-skip-permissions`
/// - Gemini: `--yolo` (gemini's own flag; skips all tool-call confirmations)
/// - Copilot: `--autopilot` (only CLI autonomous mode; no standalone --yolo flag)
/// - Crush: `--yolo` inserted at index 1 (persistent root flag, must precede `run` subcommand)
/// - Cline: `--yolo` (skips all tool-call confirmations and implies non-interactive mode)
/// When `auto` is true (and not yolo):
/// - Claude: `--permission-mode auto`
/// - Gemini: `--approval-mode=auto_edit` (auto-approves file edits/writes; prompts for shell tools)
/// - Copilot: `--autopilot` (no finer-grained auto-edit mode)
/// - Crush: `--yolo` (no intermediate mode; warning printed)
/// - Cline: `--auto-approve-all` (keeps interactive mode but auto-approves actions)
/// Both modes:
/// - Claude: if disallowed_tools non-empty, `--disallowedTools <t1>,<t2>,...`
/// - Codex: `--full-auto`; disallowed tools not supported (warning printed)
/// - Opencode: no equivalent — a warning is printed; disallowed tools not supported
/// - Maki: `--yolo` (maki's own flag to skip all permission prompts); disallowed tools not supported
/// - Gemini: disallowed tools not supported (warning printed)
/// - Copilot, Crush, Cline: disallowed tools not supported (warning printed)
pub fn append_autonomous_flags(args: &mut Vec<String>, agent: &str, yolo: bool, auto: bool, disallowed_tools: &[String]) {
    if !yolo && !auto {
        return;
    }
    let flag_name = if yolo { "--yolo" } else { "--auto" };
    match agent {
        "claude" => {
            if yolo {
                args.push("--dangerously-skip-permissions".to_string());
            } else {
                args.push("--permission-mode".to_string());
                args.push("auto".to_string());
            }
            if !disallowed_tools.is_empty() {
                args.push("--disallowedTools".to_string());
                args.push(disallowed_tools.join(","));
            }
        }
        "codex" => {
            args.push("--full-auto".to_string());
            if !disallowed_tools.is_empty() {
                eprintln!("WARNING: {}: codex does not support --disallowedTools; yoloDisallowedTools config will be ignored.", flag_name);
            }
        }
        "maki" => {
            // maki uses --yolo as its own autonomous flag (skips all permission prompts).
            // Note: the --yolo flag here is maki's flag, not amux's --yolo flag.
            args.push("--yolo".to_string());
            if !disallowed_tools.is_empty() {
                eprintln!(
                    "WARNING: {}: maki does not support --disallowedTools; yoloDisallowedTools config will be ignored.",
                    flag_name
                );
            }
        }
        "gemini" => {
            if yolo {
                // gemini's --yolo skips all tool-call confirmations.
                // Note: this is gemini's own flag, not amux's --yolo flag.
                args.push("--yolo".to_string());
            } else {
                // --auto maps to gemini's auto_edit approval mode (auto-approves file
                // edits/writes but prompts before shell tool calls — more conservative
                // than --yolo).
                args.push("--approval-mode=auto_edit".to_string());
            }
            if !disallowed_tools.is_empty() {
                eprintln!(
                    "WARNING: {}: gemini does not support --disallowedTools; yoloDisallowedTools config will be ignored.",
                    flag_name
                );
            }
        }
        "copilot" => {
            // copilot's only CLI autonomous mode is --autopilot (equivalent to yolo).
            // There is no CLI-level --yolo flag for copilot; /yolo is an interactive slash command only.
            // Both amux --yolo and --auto map to --autopilot (copilot has no finer-grained auto-edit mode).
            args.push("--autopilot".to_string());
            if !disallowed_tools.is_empty() {
                eprintln!(
                    "WARNING: {}: copilot does not support --disallowedTools via CLI flags; \
                     yoloDisallowedTools config will be ignored.",
                    flag_name
                );
            }
        }
        "crush" => {
            // crush's --yolo is a persistent root flag that MUST precede the `run`
            // subcommand: `crush --yolo run "prompt"`. Insert at index 1 (after "crush",
            // before "run") rather than pushing to the end.
            // Both --yolo and --auto map here because crush has no intermediate mode.
            args.insert(1, "--yolo".to_string());
            if !yolo {
                // --auto was requested; crush has no intermediate mode, so map to --yolo.
                eprintln!(
                    "WARNING: {}: crush has no intermediate permission mode; \
                     mapping --auto to --yolo (crush's only autonomous flag).",
                    flag_name
                );
            }
            if !disallowed_tools.is_empty() {
                eprintln!(
                    "WARNING: {}: crush does not support --disallowedTools; \
                     yoloDisallowedTools config will be ignored.",
                    flag_name
                );
            }
        }
        "cline" => {
            if yolo {
                // cline's --yolo skips all tool-call confirmations and implies non-interactive mode.
                args.push("--yolo".to_string());
            } else {
                // --auto maps to --auto-approve-all (keeps interactive mode but auto-approves actions).
                args.push("--auto-approve-all".to_string());
            }
            if !disallowed_tools.is_empty() {
                eprintln!(
                    "WARNING: {}: cline does not support --disallowedTools via CLI flags; \
                     yoloDisallowedTools config will be ignored.",
                    flag_name
                );
            }
        }
        _ => {
            // Opencode and unknown agents have no skip-permissions equivalent.
            eprintln!("WARNING: {}: agent '{}' does not support a skip-permissions flag; proceeding without it.", flag_name, agent);
            if !disallowed_tools.is_empty() {
                eprintln!("WARNING: {}: agent '{}' does not support --disallowedTools; yoloDisallowedTools config will be ignored.", flag_name, agent);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    // ─── MockRuntime for ensure_agent_available tests ─────────────────────────

    /// Minimal `AgentRuntime` stub for `ensure_agent_available` unit tests.
    /// Tracks `build_image_streaming` calls; all container-run methods panic.
    struct MockRuntime {
        /// Returned by `image_exists` for every tag query (unless overridden by absent_tags).
        project_image_exists: bool,
        /// When `false`, `build_image_streaming` returns an error.
        builds_succeed: bool,
        /// Records every image tag passed to `build_image_streaming`.
        built_tags: std::sync::Mutex<Vec<String>>,
        /// When set, tags containing any of these substrings report as absent.
        absent_tags: Option<Vec<String>>,
    }

    impl MockRuntime {
        /// Runtime where the project base image exists and builds succeed.
        fn with_project_image() -> Self {
            Self {
                project_image_exists: true,
                builds_succeed: true,
                built_tags: std::sync::Mutex::new(vec![]),
                absent_tags: None,
            }
        }

        /// Runtime where the project base image exists, but specific agent tags are absent.
        fn with_absent_agent_tags(tags: Vec<String>) -> Self {
            Self {
                project_image_exists: true,
                builds_succeed: true,
                built_tags: std::sync::Mutex::new(vec![]),
                absent_tags: Some(tags),
            }
        }

        fn built_tags(&self) -> Vec<String> {
            self.built_tags.lock().unwrap().clone()
        }
    }

    impl crate::runtime::AgentRuntime for MockRuntime {
        fn is_available(&self) -> bool { true }
        fn check_socket(&self) -> anyhow::Result<std::path::PathBuf> {
            Ok(std::path::PathBuf::from("/var/run/mock.sock"))
        }
        fn image_exists(&self, tag: &str) -> bool {
            // If a specific set of absent tags is configured, check against it.
            // Otherwise fall back to the project_image_exists flag.
            if let Some(absent) = self.absent_tags.as_ref() {
                if absent.iter().any(|t| tag.contains(t)) {
                    return false;
                }
            }
            self.project_image_exists
        }
        fn name(&self) -> &'static str { "mock" }
        fn cli_binary(&self) -> &'static str { "mock" }

        fn build_image_streaming(
            &self,
            tag: &str,
            _dockerfile: &std::path::Path,
            _context: &std::path::Path,
            _no_cache: bool,
            _on_line: &mut dyn FnMut(&str),
        ) -> anyhow::Result<String> {
            self.built_tags.lock().unwrap().push(tag.to_string());
            if self.builds_succeed {
                Ok(String::new())
            } else {
                anyhow::bail!("mock build failure")
            }
        }

        fn run_container(
            &self, _image: &str, _host_path: &str, _entrypoint: &[&str],
            _env_vars: &[(String, String)], _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool, _container_name: Option<&str>, _ssh_dir: Option<&std::path::Path>,
        ) -> anyhow::Result<()> { unreachable!("run_container not expected") }

        fn run_container_captured(
            &self, _image: &str, _host_path: &str, _entrypoint: &[&str],
            _env_vars: &[(String, String)], _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool, _container_name: Option<&str>, _ssh_dir: Option<&std::path::Path>,
        ) -> anyhow::Result<(String, String)> { unreachable!("run_container_captured not expected") }

        fn run_container_at_path(
            &self, _image: &str, _host_path: &str, _container_path: &str, _working_dir: &str,
            _entrypoint: &[&str], _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>, _allow_docker: bool,
            _container_name: Option<&str>,
        ) -> anyhow::Result<()> { unreachable!() }

        fn run_container_captured_at_path(
            &self, _image: &str, _host_path: &str, _container_path: &str, _working_dir: &str,
            _entrypoint: &[&str], _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>, _allow_docker: bool,
        ) -> anyhow::Result<(String, String)> { unreachable!() }

        fn run_container_detached(
            &self, _image: &str, _host_path: &str, _container_path: &str, _working_dir: &str,
            _container_name: Option<&str>, _env_vars: Vec<(String, String)>, _allow_docker: bool,
            _host_settings: Option<&crate::runtime::HostSettings>,
        ) -> anyhow::Result<String> { unreachable!() }

        fn start_container(&self, _id: &str) -> anyhow::Result<()> { unreachable!() }
        fn stop_container(&self, _id: &str) -> anyhow::Result<()> { unreachable!() }
        fn remove_container(&self, _id: &str) -> anyhow::Result<()> { unreachable!() }
        fn is_container_running(&self, _id: &str) -> bool { unreachable!() }

        fn find_stopped_container(
            &self, _name: &str, _image: &str,
        ) -> Option<crate::runtime::StoppedContainerInfo> { unreachable!() }

        fn list_running_containers_by_prefix(&self, _prefix: &str) -> Vec<String> {
            unreachable!()
        }

        fn list_running_containers_with_ids_by_prefix(
            &self, _prefix: &str,
        ) -> Vec<(String, String)> { unreachable!() }

        fn get_container_workspace_mount(&self, _name: &str) -> Option<String> { unreachable!() }

        fn query_container_stats(
            &self, _name: &str,
        ) -> Option<crate::runtime::ContainerStats> { unreachable!() }

        fn build_run_args_pty(
            &self, _image: &str, _host_path: &str, _entrypoint: &[&str],
            _env_vars: &[(String, String)], _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool, _container_name: Option<&str>, _ssh_dir: Option<&std::path::Path>,
        ) -> Vec<String> { unreachable!() }

        fn build_run_args_pty_display(
            &self, _image: &str, _host_path: &str, _entrypoint: &[&str],
            _env_vars: &[(String, String)], _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool, _container_name: Option<&str>, _ssh_dir: Option<&std::path::Path>,
        ) -> Vec<String> { unreachable!() }

        fn build_run_args_pty_at_path(
            &self, _image: &str, _host_path: &str, _container_path: &str, _working_dir: &str,
            _entrypoint: &[&str], _env_vars: &[(String, String)],
            _host_settings: Option<&crate::runtime::HostSettings>, _allow_docker: bool,
            _container_name: Option<&str>,
        ) -> Vec<String> { unreachable!() }

        fn build_exec_args_pty(
            &self, _container_id: &str, _working_dir: &str, _entrypoint: &[&str],
            _env_vars: &[(String, String)],
        ) -> Vec<String> { unreachable!() }

        fn build_run_args_display(
            &self, _image: &str, _host_path: &str, _entrypoint: &[&str],
            _env_vars: &[(String, String)], _host_settings: Option<&crate::runtime::HostSettings>,
            _allow_docker: bool, _container_name: Option<&str>, _ssh_dir: Option<&std::path::Path>,
        ) -> Vec<String> { unreachable!() }
    }

    // ─── ensure_agent_available tests ────────────────────────────────────────

    #[tokio::test]
    async fn ensure_agent_available_returns_true_when_dockerfile_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Create .amux/Dockerfile.codex so the agent is already set up.
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(amux_dir.join("Dockerfile.codex"), "FROM ubuntu\n").unwrap();

        let runtime = MockRuntime::with_project_image();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);

        let result = ensure_agent_available(
            tmp.path(),
            "codex",
            &sink,
            &runtime,
            |_| panic!("ask_fn must not be called when Dockerfile already exists"),
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true, "must return true when Dockerfile already exists");
    }

    #[test]
    fn build_agent_image_builds_when_dockerfile_exists_but_image_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Create .amux/Dockerfile.codex so the agent Dockerfile is already present.
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();
        std::fs::write(amux_dir.join("Dockerfile.codex"), "FROM ubuntu
").unwrap();

        // Project base image exists, but agent image is absent.
        let runtime = MockRuntime::with_absent_agent_tags(vec!["codex".to_string()]);
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);

        let result = build_agent_image(
            tmp.path(),
            "codex",
            &sink,
            &runtime,
        );

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), true, "must return true after building image from existing Dockerfile");
        // Verify the agent image was actually built.
        let tags = runtime.built_tags();
        assert!(tags.iter().any(|t| t.contains("codex")), "agent image must be built; got built tags: {:?}", tags);
    }

    #[tokio::test]
    async fn ensure_agent_available_returns_false_on_http_connection_failure() {
        // When the HTTP download fails (connection refused), ensure_agent_available must
        // return Ok(false) and not leave a partial Dockerfile on disk.
        let tmp = tempfile::TempDir::new().unwrap();
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();

        let runtime = MockRuntime::with_project_image();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);

        // Port 0 is guaranteed to be unreachable — produces a connection-refused error.
        let result = ensure_agent_available_inner(
            tmp.path(),
            "codex",
            &sink,
            &runtime,
            |_| Ok(true), // user accepts download
            &[("codex", "http://localhost:0/Dockerfile.codex")],
        )
        .await;

        assert!(result.is_ok(), "connection failure must return Ok, not Err; got: {:?}", result);
        assert_eq!(result.unwrap(), false, "connection failure must return Ok(false)");
        assert!(
            !amux_dir.join("Dockerfile.codex").exists(),
            "no partial Dockerfile must be left on connection failure"
        );
    }

    #[tokio::test]
    async fn ensure_agent_available_build_failure_returns_false_and_removes_dockerfile() {
        // When the image build fails after a successful download, ensure_agent_available
        // must return Ok(false) and remove the partial Dockerfile from .amux/.
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();

        // Spin up a minimal HTTP server that serves a dummy Dockerfile.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/Dockerfile.codex", addr);

        let _server = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let body = b"FROM ubuntu:22.04\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(body).await;
            }
        });

        let runtime = MockRuntime {
            project_image_exists: true,
            builds_succeed: false,
            built_tags: std::sync::Mutex::new(vec![]),
            absent_tags: None,
        };
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);

        let result = ensure_agent_available_inner(
            tmp.path(),
            "codex",
            &sink,
            &runtime,
            |_| Ok(true),
            &[("codex", url.as_str())],
        )
        .await;

        assert!(result.is_ok(), "build failure must return Ok, not Err");
        assert_eq!(result.unwrap(), false, "build failure must return Ok(false)");
        assert!(
            !amux_dir.join("Dockerfile.codex").exists(),
            "partial Dockerfile must be removed on build failure"
        );
    }

    #[tokio::test]
    async fn ensure_agent_available_substitutes_amux_base_image_placeholder() {
        // When the downloaded Dockerfile contains {{AMUX_BASE_IMAGE}}, the saved file
        // must have that placeholder replaced with the project image tag derived from
        // the git root, so the agent image layers on top of the correct base image.
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let amux_dir = tmp.path().join(".amux");
        std::fs::create_dir_all(&amux_dir).unwrap();

        // Serve a Dockerfile template that uses the {{AMUX_BASE_IMAGE}} placeholder.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/Dockerfile.codex", addr);

        let _server = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let body = b"FROM {{AMUX_BASE_IMAGE}}\nRUN echo hello\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.write_all(body).await;
            }
        });

        let runtime = MockRuntime::with_project_image();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);

        let result = ensure_agent_available_inner(
            tmp.path(),
            "codex",
            &sink,
            &runtime,
            |_| Ok(true),
            &[("codex", url.as_str())],
        )
        .await;

        assert!(result.is_ok(), "substitution must not return Err; got: {:?}", result);
        assert_eq!(result.unwrap(), true, "must return Ok(true) on success");

        // The saved Dockerfile must have {{AMUX_BASE_IMAGE}} replaced with the
        // project base image tag (amux-{project_name}:latest).
        let saved = std::fs::read_to_string(amux_dir.join("Dockerfile.codex")).unwrap();
        let expected_base = crate::runtime::project_image_tag(tmp.path());
        assert!(
            saved.contains(&expected_base),
            "saved Dockerfile must contain project image tag '{}'; got:\n{}",
            expected_base, saved
        );
        assert!(
            !saved.contains("{{AMUX_BASE_IMAGE}}"),
            "saved Dockerfile must not retain the placeholder; got:\n{}",
            saved
        );
    }

    #[tokio::test]
    async fn ensure_agent_available_returns_false_when_user_declines() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No dockerfiles exist.

        let runtime = MockRuntime::with_project_image();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);

        let result = ensure_agent_available(
            tmp.path(),
            "codex",
            &sink,
            &runtime,
            |_| Ok(false), // user declines
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), false, "must return false when user declines");
        // No Dockerfile must have been created.
        assert!(
            !tmp.path().join(".amux").join("Dockerfile.codex").exists(),
            "Dockerfile must not be created when user declines"
        );
        // No image build must have been triggered.
        assert_eq!(
            runtime.built_tags(),
            Vec::<String>::new(),
            "build_image_streaming must not be called when user declines"
        );
    }

    #[tokio::test]
    async fn ensure_agent_available_returns_error_for_unknown_agent_after_accept() {
        // When the user accepts setup but the agent has no known Dockerfile URL,
        // the function must return an error describing the problem.
        let tmp = tempfile::TempDir::new().unwrap();
        // No dockerfiles exist.

        let runtime = MockRuntime::with_project_image();
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);

        let result = ensure_agent_available(
            tmp.path(),
            "unknown-bot",
            &sink,
            &runtime,
            |_| Ok(true), // user accepts
        )
        .await;

        assert!(
            result.is_err(),
            "must return an error when no Dockerfile URL is known for the agent"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unknown-bot"),
            "error must mention the unknown agent name; got: {msg}"
        );
    }

    // --- append_autonomous_flags tests ---

    #[test]
    fn append_autonomous_flags_noop_when_yolo_false() {
        let mut args = vec!["claude".to_string()];
        append_autonomous_flags(&mut args, "claude", false, false, &[]);
        assert_eq!(args, vec!["claude"]);
    }

    #[test]
    fn append_autonomous_flags_claude_adds_skip_permissions() {
        let mut args = vec!["claude".to_string()];
        append_autonomous_flags(&mut args, "claude", true, false, &[]);
        assert!(
            args.contains(&"--dangerously-skip-permissions".to_string()),
            "claude must receive --dangerously-skip-permissions"
        );
    }

    #[test]
    fn append_autonomous_flags_claude_no_disallowed_tools_skips_flag() {
        let mut args = vec!["claude".to_string()];
        append_autonomous_flags(&mut args, "claude", true, false, &[]);
        assert!(
            !args.contains(&"--disallowedTools".to_string()),
            "--disallowedTools must not appear when the list is empty"
        );
    }

    #[test]
    fn append_autonomous_flags_claude_with_disallowed_tools() {
        let mut args = vec!["claude".to_string()];
        let tools = vec!["Bash".to_string(), "computer".to_string()];
        append_autonomous_flags(&mut args, "claude", true, false, &tools);
        let dt_idx = args
            .iter()
            .position(|a| a == "--disallowedTools")
            .expect("--disallowedTools flag missing");
        assert_eq!(args[dt_idx + 1], "Bash,computer");
    }

    #[test]
    fn append_autonomous_flags_codex_adds_full_auto() {
        let mut args = vec!["codex".to_string()];
        append_autonomous_flags(&mut args, "codex", true, false, &[]);
        assert!(args.contains(&"--full-auto".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn append_autonomous_flags_codex_no_disallowed_tools_flag() {
        // codex does not support --disallowedTools; the flag must never appear
        let mut args = vec!["codex".to_string()];
        let tools = vec!["Bash".to_string()];
        append_autonomous_flags(&mut args, "codex", true, false, &tools);
        assert!(!args.contains(&"--disallowedTools".to_string()));
    }

    #[test]
    fn append_autonomous_flags_opencode_no_skip_permissions_flag() {
        // opencode has no skip-permissions equivalent; args must be unchanged
        let mut args = vec!["opencode".to_string()];
        append_autonomous_flags(&mut args, "opencode", true, false, &[]);
        assert_eq!(args, vec!["opencode"]);
    }

    #[test]
    fn append_autonomous_flags_opencode_no_disallowed_tools_flag() {
        let mut args = vec!["opencode".to_string()];
        let tools = vec!["Bash".to_string()];
        append_autonomous_flags(&mut args, "opencode", true, false, &tools);
        assert!(!args.contains(&"--disallowedTools".to_string()));
        assert_eq!(args, vec!["opencode"]);
    }

    #[test]
    fn append_autonomous_flags_noop_when_both_false() {
        let mut args = vec!["claude".to_string()];
        append_autonomous_flags(&mut args, "claude", false, false, &[]);
        assert_eq!(args, vec!["claude"]);
    }

    #[test]
    fn append_autonomous_flags_auto_claude_adds_permission_mode_auto() {
        let mut args = vec!["claude".to_string()];
        append_autonomous_flags(&mut args, "claude", false, true, &[]);
        assert!(
            args.contains(&"--permission-mode".to_string()),
            "claude in auto mode must receive --permission-mode"
        );
        assert!(args.contains(&"auto".to_string()), "auto value must be present");
        assert!(
            !args.contains(&"--dangerously-skip-permissions".to_string()),
            "--dangerously-skip-permissions must NOT appear in auto mode"
        );
    }

    #[test]
    fn append_autonomous_flags_auto_claude_with_disallowed_tools() {
        let mut args = vec!["claude".to_string()];
        let tools = vec!["Bash".to_string()];
        append_autonomous_flags(&mut args, "claude", false, true, &tools);
        assert!(args.contains(&"--disallowedTools".to_string()));
    }

    #[test]
    fn append_autonomous_flags_yolo_takes_precedence_over_auto() {
        // When both are true, yolo wins (uses --dangerously-skip-permissions).
        let mut args = vec!["claude".to_string()];
        append_autonomous_flags(&mut args, "claude", true, true, &[]);
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"auto".to_string()));
    }

    #[test]
    fn append_autonomous_flags_maki_adds_yolo_flag() {
        let mut args = vec!["maki".to_string()];
        append_autonomous_flags(&mut args, "maki", true, false, &[]);
        assert!(args.contains(&"--yolo".to_string()), "maki must receive --yolo in yolo mode");
    }

    #[test]
    fn append_autonomous_flags_maki_never_adds_disallowed_tools_flag() {
        // maki does not support --disallowedTools; it must never appear regardless of the list.
        let mut args = vec!["maki".to_string()];
        let tools = vec!["Bash".to_string(), "computer".to_string()];
        append_autonomous_flags(&mut args, "maki", true, false, &tools);
        assert!(
            !args.contains(&"--disallowedTools".to_string()),
            "--disallowedTools must never appear for maki"
        );
        assert!(args.contains(&"--yolo".to_string()), "--yolo must still be appended");
    }

    #[test]
    fn append_autonomous_flags_maki_prints_warning_when_disallowed_tools_nonempty() {
        // The warning is emitted via eprintln! and cannot be trivially captured in a unit test
        // without a custom stderr-redirect harness. This test verifies the code path compiles
        // and does not panic.
        let mut args = vec!["maki".to_string()];
        let tools = vec!["Bash".to_string()];
        append_autonomous_flags(&mut args, "maki", true, false, &tools);
    }

    #[test]
    fn append_autonomous_flags_maki_no_disallowed_tools_exact_args() {
        // When disallowed_tools is empty, exactly ["maki", "--yolo"] must result.
        let mut args = vec!["maki".to_string()];
        append_autonomous_flags(&mut args, "maki", true, false, &[]);
        assert_eq!(args, vec!["maki", "--yolo"]);
    }

    // --- gemini autonomous flags ---

    #[test]
    fn append_autonomous_flags_gemini_yolo_adds_yolo_flag() {
        let mut args = vec!["gemini".to_string()];
        append_autonomous_flags(&mut args, "gemini", true, false, &[]);
        assert!(args.contains(&"--yolo".to_string()), "gemini must receive --yolo in yolo mode");
    }

    #[test]
    fn append_autonomous_flags_gemini_yolo_never_adds_disallowed_tools_flag() {
        // gemini does not support --disallowedTools; the flag must never appear.
        let mut args = vec!["gemini".to_string()];
        let tools = vec!["Bash".to_string(), "computer".to_string()];
        append_autonomous_flags(&mut args, "gemini", true, false, &tools);
        assert!(
            !args.contains(&"--disallowedTools".to_string()),
            "--disallowedTools must never appear for gemini"
        );
        assert!(args.contains(&"--yolo".to_string()), "--yolo must still be appended");
    }

    #[test]
    fn append_autonomous_flags_gemini_auto_adds_approval_mode_auto_edit() {
        let mut args = vec!["gemini".to_string()];
        append_autonomous_flags(&mut args, "gemini", false, true, &[]);
        assert!(
            args.contains(&"--approval-mode=auto_edit".to_string()),
            "gemini in auto mode must receive --approval-mode=auto_edit"
        );
        assert!(
            !args.contains(&"--dangerously-skip-permissions".to_string()),
            "--dangerously-skip-permissions must NOT appear for gemini"
        );
        assert!(
            !args.contains(&"--yolo".to_string()),
            "--yolo must NOT appear in auto mode"
        );
    }

    #[test]
    fn append_autonomous_flags_gemini_yolo_with_nonempty_disallowed_tools_prints_warning() {
        // Warning is emitted via eprintln! — verify the code path compiles and does not panic.
        let mut args = vec!["gemini".to_string()];
        let tools = vec!["Bash".to_string()];
        append_autonomous_flags(&mut args, "gemini", true, false, &tools);
        // --yolo must still be appended despite the warning.
        assert!(args.contains(&"--yolo".to_string()));
        assert!(!args.contains(&"--disallowedTools".to_string()));
    }

    #[test]
    fn append_autonomous_flags_gemini_yolo_takes_precedence_over_auto() {
        // When both yolo and auto are true, yolo wins for gemini.
        let mut args = vec!["gemini".to_string()];
        append_autonomous_flags(&mut args, "gemini", true, true, &[]);
        assert!(args.contains(&"--yolo".to_string()), "--yolo must appear when yolo=true");
        assert!(
            !args.contains(&"--approval-mode=auto_edit".to_string()),
            "--approval-mode=auto_edit must NOT appear when yolo=true"
        );
    }

    // --- copilot autonomous flags ---

    #[test]
    fn append_autonomous_flags_copilot_yolo_adds_autopilot() {
        let mut args = vec!["copilot".to_string()];
        append_autonomous_flags(&mut args, "copilot", true, false, &[]);
        assert!(
            args.contains(&"--autopilot".to_string()),
            "copilot must receive --autopilot in yolo mode"
        );
        assert!(
            !args.contains(&"--yolo".to_string()),
            "copilot must NOT receive --yolo (no such CLI flag for copilot)"
        );
    }

    #[test]
    fn append_autonomous_flags_copilot_auto_adds_autopilot() {
        // Both --yolo and --auto map to --autopilot for copilot (no finer-grained mode).
        let mut args = vec!["copilot".to_string()];
        append_autonomous_flags(&mut args, "copilot", false, true, &[]);
        assert!(
            args.contains(&"--autopilot".to_string()),
            "copilot must receive --autopilot in auto mode"
        );
    }

    #[test]
    fn append_autonomous_flags_copilot_never_adds_disallowed_tools_flag() {
        // copilot does not support --disallowedTools via CLI flags; the flag must never appear.
        let mut args = vec!["copilot".to_string()];
        let tools = vec!["Bash".to_string(), "computer".to_string()];
        append_autonomous_flags(&mut args, "copilot", true, false, &tools);
        assert!(
            !args.contains(&"--disallowedTools".to_string()),
            "--disallowedTools must never appear for copilot"
        );
        assert!(
            args.contains(&"--autopilot".to_string()),
            "--autopilot must still be appended despite disallowed_tools warning"
        );
    }

    #[test]
    fn append_autonomous_flags_copilot_yolo_with_disallowed_tools_prints_warning_and_still_adds_autopilot() {
        // Warning is emitted via eprintln! — verify the code path compiles and does not panic.
        let mut args = vec!["copilot".to_string()];
        let tools = vec!["bash".to_string()];
        append_autonomous_flags(&mut args, "copilot", true, false, &tools);
        // --autopilot must still be present despite the warning.
        assert_eq!(args, vec!["copilot", "--autopilot"]);
    }

    // --- crush autonomous flags ---

    #[test]
    fn append_autonomous_flags_crush_yolo_inserts_at_index_1() {
        // --yolo is a persistent root flag that must precede the `run` subcommand.
        let mut args = vec!["crush".to_string(), "run".to_string()];
        append_autonomous_flags(&mut args, "crush", true, false, &[]);
        assert_eq!(args, vec!["crush", "--yolo", "run"]);
    }

    #[test]
    fn append_autonomous_flags_crush_yolo_interactive_form() {
        // Interactive base: just `["crush"]`.
        let mut args = vec!["crush".to_string()];
        append_autonomous_flags(&mut args, "crush", true, false, &[]);
        assert_eq!(args, vec!["crush", "--yolo"]);
    }

    #[test]
    fn append_autonomous_flags_crush_yolo_with_prompt_inserts_at_index_1() {
        // With prompt: `["crush", "run", "prompt"]` → `["crush", "--yolo", "run", "prompt"]`.
        let mut args = vec!["crush".to_string(), "run".to_string(), "fix bug".to_string()];
        append_autonomous_flags(&mut args, "crush", true, false, &[]);
        assert_eq!(args, vec!["crush", "--yolo", "run", "fix bug"]);
    }

    #[test]
    fn append_autonomous_flags_crush_auto_inserts_yolo_at_index_1() {
        // crush has no intermediate mode; --auto maps to --yolo (with a warning).
        let mut args = vec!["crush".to_string(), "run".to_string()];
        append_autonomous_flags(&mut args, "crush", false, true, &[]);
        // --yolo must be inserted at index 1, not pushed to the end.
        assert_eq!(args, vec!["crush", "--yolo", "run"]);
    }

    #[test]
    fn append_autonomous_flags_crush_disallowed_tools_warning_yolo_still_inserted() {
        // Warning is emitted; --yolo must still be inserted at index 1.
        let mut args = vec!["crush".to_string(), "run".to_string()];
        let tools = vec!["bash".to_string()];
        append_autonomous_flags(&mut args, "crush", true, false, &tools);
        assert_eq!(
            args,
            vec!["crush", "--yolo", "run"],
            "--yolo must be inserted at index 1 even when disallowed_tools warning is present"
        );
        assert!(
            !args.contains(&"--disallowedTools".to_string()),
            "--disallowedTools must never appear for crush"
        );
    }

    // --- cline autonomous flags ---

    #[test]
    fn append_autonomous_flags_cline_yolo_appends_yolo_flag() {
        let mut args = vec!["cline".to_string(), "task".to_string(), "--json".to_string()];
        append_autonomous_flags(&mut args, "cline", true, false, &[]);
        assert!(
            args.contains(&"--yolo".to_string()),
            "cline must receive --yolo in yolo mode"
        );
        assert!(
            !args.contains(&"--auto-approve-all".to_string()),
            "--auto-approve-all must NOT appear in yolo mode"
        );
    }

    #[test]
    fn append_autonomous_flags_cline_auto_appends_auto_approve_all() {
        // --auto maps to --auto-approve-all for cline (keeps interactive mode).
        let mut args = vec!["cline".to_string(), "task".to_string()];
        append_autonomous_flags(&mut args, "cline", false, true, &[]);
        assert!(
            args.contains(&"--auto-approve-all".to_string()),
            "cline must receive --auto-approve-all in auto mode"
        );
        assert!(
            !args.contains(&"--yolo".to_string()),
            "--yolo must NOT appear in auto mode for cline"
        );
    }

    #[test]
    fn append_autonomous_flags_cline_yolo_wins_over_auto() {
        // When both yolo and auto are true, yolo wins: --yolo appended, not --auto-approve-all.
        let mut args = vec!["cline".to_string(), "task".to_string()];
        append_autonomous_flags(&mut args, "cline", true, true, &[]);
        assert!(args.contains(&"--yolo".to_string()), "--yolo must appear when yolo=true");
        assert!(
            !args.contains(&"--auto-approve-all".to_string()),
            "--auto-approve-all must NOT appear when yolo=true"
        );
    }

    #[test]
    fn append_autonomous_flags_cline_disallowed_tools_no_flag_forwarded() {
        // cline does not support --disallowedTools; warning emitted but flag never added.
        let mut args = vec!["cline".to_string(), "task".to_string()];
        let tools = vec!["Bash".to_string()];
        append_autonomous_flags(&mut args, "cline", true, false, &tools);
        assert!(
            !args.contains(&"--disallowedTools".to_string()),
            "--disallowedTools must never appear for cline"
        );
        assert!(args.contains(&"--yolo".to_string()), "--yolo must still be appended");
    }

    // --- append_model_flag tests (work item 0055) ---

    #[test]
    fn append_model_flag_claude_appends_model_flag() {
        let mut args = vec!["claude".to_string()];
        append_model_flag(&mut args, "claude", "claude-opus-4-6");
        assert_eq!(args, vec!["claude", "--model", "claude-opus-4-6"]);
    }

    #[test]
    fn append_model_flag_codex_appends_model_flag() {
        let mut args = vec!["codex".to_string()];
        append_model_flag(&mut args, "codex", "gpt-4o");
        assert_eq!(args, vec!["codex", "--model", "gpt-4o"]);
    }

    #[test]
    fn append_model_flag_gemini_appends_model_flag() {
        let mut args = vec!["gemini".to_string()];
        append_model_flag(&mut args, "gemini", "gemini-2.0-flash");
        assert_eq!(args, vec!["gemini", "--model", "gemini-2.0-flash"]);
    }

    #[test]
    fn append_model_flag_opencode_appends_provider_slash_model() {
        // opencode requires `provider/model` format; amux passes the value through verbatim.
        let mut args = vec!["opencode".to_string()];
        append_model_flag(&mut args, "opencode", "anthropic/claude-3-5-sonnet");
        assert_eq!(
            args,
            vec!["opencode", "--model", "anthropic/claude-3-5-sonnet"]
        );
    }

    #[test]
    fn append_model_flag_maki_appends_provider_slash_model() {
        // maki accepts `provider/model-id`; amux passes the value through verbatim.
        let mut args = vec!["maki".to_string()];
        append_model_flag(&mut args, "maki", "anthropic/claude-opus-4-6");
        assert_eq!(args, vec!["maki", "--model", "anthropic/claude-opus-4-6"]);
    }

    #[test]
    fn append_model_flag_crush_accepts_provider_slash_model() {
        // crush accepts either a bare model ID or `provider/model` to disambiguate.
        let mut args = vec!["crush".to_string(), "run".to_string()];
        append_model_flag(&mut args, "crush", "openrouter/anthropic/claude-sonnet-4");
        assert_eq!(
            args,
            vec![
                "crush",
                "run",
                "--model",
                "openrouter/anthropic/claude-sonnet-4"
            ]
        );
    }

    #[test]
    fn append_model_flag_unknown_agent_does_not_append_flag() {
        // Unknown agents print a warning and skip the flag; args must be unchanged.
        let mut args = vec!["unknown-bot".to_string()];
        append_model_flag(&mut args, "unknown-bot", "some-model");
        assert_eq!(
            args,
            vec!["unknown-bot"],
            "unknown agent must not receive --model"
        );
    }

    #[test]
    fn append_model_flag_copilot_does_not_append_flag() {
        // copilot selects models via the /model interactive slash command, not a CLI flag.
        // append_model_flag must warn and leave args unchanged.
        let mut args = vec!["copilot".to_string()];
        append_model_flag(&mut args, "copilot", "gpt-4o");
        assert_eq!(
            args,
            vec!["copilot"],
            "copilot must not receive --model (model selection is via /model slash command)"
        );
    }

    #[test]
    fn append_model_flag_crush_appends_model_flag() {
        // crush supports --model on its `run` subcommand.
        let mut args = vec!["crush".to_string(), "run".to_string()];
        append_model_flag(&mut args, "crush", "claude-opus-4-6");
        assert_eq!(args, vec!["crush", "run", "--model", "claude-opus-4-6"]);
    }

    #[test]
    fn append_model_flag_cline_appends_model_flag() {
        // cline supports --model as a direct CLI flag.
        let mut args = vec!["cline".to_string(), "task".to_string()];
        append_model_flag(&mut args, "cline", "claude-opus-4-6");
        assert_eq!(args, vec!["cline", "task", "--model", "claude-opus-4-6"]);
    }

    #[test]
    fn none_model_does_not_produce_extra_args() {
        // When model is None the `if let Some(m) = model` guard in run_agent_with_sink
        // skips append_model_flag entirely. Verify the guard logic directly.
        let mut args = vec!["claude".to_string()];
        let model: Option<&str> = None;
        if let Some(m) = model {
            append_model_flag(&mut args, "claude", m);
        }
        assert_eq!(
            args,
            vec!["claude"],
            "None model must not produce any additional args"
        );
    }

    #[tokio::test]
    async fn run_agent_with_sink_fails_without_git_root() {
        let (tx, _rx) = unbounded_channel();
        let sink = OutputSink::Channel(tx);
        let entrypoint = vec!["claude".to_string()];
        // Run from a temp dir with no git repo.
        let tmp = tempfile::TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let runtime = crate::runtime::DockerRuntime::new();
        let result = run_agent_with_sink(
            entrypoint,
            "test",
            &sink,
            Some(tmp.path().to_path_buf()),
            vec![],
            false,
            None,
            false,
            false,
            None,
            None,
            None,
            &runtime,
            None,
        )
        .await;

        std::env::set_current_dir(original_dir).unwrap();
        assert!(result.is_err());
    }
}
