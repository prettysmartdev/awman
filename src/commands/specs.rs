use crate::commands::agent::run_agent_with_sink;
use crate::commands::auth::resolve_auth;
use crate::commands::implement::{confirm_mount_scope_stdin, find_work_item, parse_work_item};
use crate::commands::init_flow::{find_git_root, find_git_root_from};
#[cfg(not(test))]
use crate::commands::new::{is_vscode_terminal, open_in_vscode};
use crate::commands::new::{create_file_return_number, prompt_kind, prompt_title, WorkItemKind};
use crate::commands::output::OutputSink;
use crate::config::load_repo_config;
use crate::runtime::HostSettings;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const INTERVIEW_PROMPT_TEMPLATE: &str = "Work item {number} template has been created for \
{kind}: {title}. Help complete the work item based on the following summary, making sure to \
include 1-3 concise user stories, detailed implementation plan, edge case considerations, \
test plan, and codebase integration tips. Only edit the work item markdown file, follow the \
template format. Do not edit any other files. Do not summarize your work at the end, let the \
user view the file themselves.\n\nSummary:\n{summary}";

const AMEND_PROMPT_TEMPLATE: &str = "Work item {number} is complete. Review the work that has \
been done in the codebase and compare it against the work item markdown file. If needed, amend \
the work item to ensure it matches the final implementation, ensuring completeness and \
correctness. Only edit the work item markdown file. Be concise and prefer leaving existing text \
as-is unless it is factually incorrect. Add new details if needed. Summarize the implementation \
and any corrections or changes that were needed to achieve the desired result in a new \
`Agent implementation notes` section at the bottom of the file.";

/// Build the interview prompt for a new work item.
pub fn interview_prompt(number: u32, kind: &WorkItemKind, title: &str, summary: &str) -> String {
    INTERVIEW_PROMPT_TEMPLATE
        .replace("{number}", &format!("{:04}", number))
        .replace("{kind}", kind.as_str())
        .replace("{title}", title)
        .replace("{summary}", summary)
}

/// Build the amend prompt for a completed work item.
pub fn amend_prompt(number: u32) -> String {
    AMEND_PROMPT_TEMPLATE.replace("{number}", &format!("{:04}", number))
}

/// Build the interactive agent entrypoint for the interview command.
pub fn interview_agent_entrypoint(
    agent: &str,
    number: u32,
    kind: &WorkItemKind,
    title: &str,
    summary: &str,
) -> Vec<String> {
    let prompt = interview_prompt(number, kind, title, summary);
    match agent {
        "claude" => vec!["claude".to_string(), prompt],
        "codex" => vec!["codex".to_string(), prompt],
        "opencode" => vec!["opencode".to_string(), "run".to_string(), prompt],
        _ => vec![agent.to_string(), prompt],
    }
}

/// Build the non-interactive agent entrypoint for the interview command.
pub fn interview_agent_entrypoint_non_interactive(
    agent: &str,
    number: u32,
    kind: &WorkItemKind,
    title: &str,
    summary: &str,
) -> Vec<String> {
    let prompt = interview_prompt(number, kind, title, summary);
    match agent {
        "claude" => vec!["claude".to_string(), "-p".to_string(), prompt],
        "codex" => vec!["codex".to_string(), "exec".to_string(), prompt],
        "opencode" => vec!["opencode".to_string(), "run".to_string(), prompt],
        _ => vec![agent.to_string(), prompt],
    }
}

/// Build the interactive agent entrypoint for the amend command.
pub fn amend_agent_entrypoint(agent: &str, number: u32) -> Vec<String> {
    let prompt = amend_prompt(number);
    match agent {
        "claude" => vec!["claude".to_string(), prompt],
        "codex" => vec!["codex".to_string(), prompt],
        "opencode" => vec!["opencode".to_string(), "run".to_string(), prompt],
        _ => vec![agent.to_string(), prompt],
    }
}

/// Build the non-interactive agent entrypoint for the amend command.
pub fn amend_agent_entrypoint_non_interactive(agent: &str, number: u32) -> Vec<String> {
    let prompt = amend_prompt(number);
    match agent {
        "claude" => vec!["claude".to_string(), "-p".to_string(), prompt],
        "codex" => vec!["codex".to_string(), "exec".to_string(), prompt],
        "opencode" => vec!["opencode".to_string(), "run".to_string(), prompt],
        _ => vec![agent.to_string(), prompt],
    }
}

/// Prompt the user to enter a summary (single-line stdin read).
fn prompt_summary(out: &OutputSink) -> Result<String> {
    out.println("Your code agent will assist with completing this work item.".to_string());
    out.print("Enter a brief summary of this work item: ");

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;

    let summary = input.trim().to_string();
    if summary.is_empty() {
        bail!("Summary cannot be empty.");
    }
    Ok(summary)
}

/// Load the repo config and return the agent name string.
fn agent_name_from_config(git_root: &Path) -> Result<String> {
    let config = load_repo_config(git_root)?;
    Ok(config.agent.as_deref().unwrap_or("claude").to_string())
}

/// CLI entry point for `amux specs new`.
pub async fn run_new(interview: bool) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let global_config = crate::config::load_global_config().unwrap_or_default();
    let runtime = crate::runtime::resolve_runtime(&global_config)?;
    run_new_with_sink(&OutputSink::Stdout, None, None, &cwd, interview, None, &*runtime).await
}

/// Shared logic for `specs new` — used by both CLI and TUI.
pub async fn run_new_with_sink(
    out: &OutputSink,
    kind: Option<WorkItemKind>,
    title: Option<String>,
    cwd: &Path,
    interview: bool,
    summary: Option<String>,
    runtime: &dyn crate::runtime::AgentRuntime,
) -> Result<()> {
    if !interview {
        return crate::commands::new::run_with_sink(out, kind, title, cwd).await;
    }

    // Interview mode: create file, prompt summary, launch agent.
    let kind = match kind {
        Some(k) => k,
        None => prompt_kind(out)?,
    };
    let title = match title {
        Some(t) => t,
        None => prompt_title(out)?,
    };

    let number = create_file_return_number(out, kind.clone(), title.clone(), cwd).await?;

    out.println("Your code agent will assist with completing this work item.".to_string());

    let summary = match summary {
        Some(s) => s,
        None => prompt_summary(out)?,
    };

    let git_root = find_git_root_from(cwd).context("Not inside a Git repository")?;
    let mount_path = confirm_mount_scope_stdin(&git_root)?;
    let agent = agent_name_from_config(&git_root)?;
    let credentials = resolve_auth(&git_root, &agent)?;
    let host_settings = crate::passthrough::passthrough_for_agent(&agent).prepare_host_settings();

    let entrypoint = interview_agent_entrypoint(&agent, number, &kind, &title, &summary);

    let status = format!(
        "Running interview agent for work item {:04} with agent '{}'",
        number, agent
    );

    run_agent_with_sink(
        entrypoint,
        &status,
        out,
        Some(mount_path),
        credentials.env_vars,
        false,
        host_settings.as_ref(),
        false,
        false,
        None,
        None,
        None,
        runtime,
        None,
    )
    .await?;

    // Open the work item file in VS Code after the agent finishes.
    #[cfg(not(test))]
    if is_vscode_terminal() {
        if let Some(path) = find_work_item(&git_root, number).ok() {
            open_in_vscode(&path);
            out.println(format!("Opened work item {:04} in VS Code.", number));
        }
    }

    Ok(())
}

/// CLI entry point for `amux specs amend <work_item>`.
pub async fn run_amend(
    work_item_str: &str,
    non_interactive: bool,
    allow_docker: bool,
    runtime: std::sync::Arc<dyn crate::runtime::AgentRuntime>,
) -> Result<()> {
    let work_item = parse_work_item(work_item_str)?;
    let git_root = find_git_root().context("Not inside a Git repository")?;
    let mount_path = confirm_mount_scope_stdin(&git_root)?;
    let agent = agent_name_from_config(&git_root)?;

    let _ = find_work_item(&git_root, work_item)?;

    let credentials = resolve_auth(&git_root, &agent)?;
    let host_settings = crate::passthrough::passthrough_for_agent(&agent).prepare_host_settings();

    let entrypoint = if non_interactive {
        amend_agent_entrypoint_non_interactive(&agent, work_item)
    } else {
        amend_agent_entrypoint(&agent, work_item)
    };

    let status = format!(
        "Amending work item {:04} with agent '{}'",
        work_item, agent
    );

    run_agent_with_sink(
        entrypoint,
        &status,
        &OutputSink::Stdout,
        Some(mount_path),
        credentials.env_vars,
        non_interactive,
        host_settings.as_ref(),
        allow_docker,
        false,
        None,
        None,
        None,
        &*runtime,
        None,
    )
    .await
}

/// Shared logic for `specs amend` — used by both CLI and TUI.
pub async fn run_with_sink_amend(
    work_item: u32,
    out: &OutputSink,
    mount_override: Option<PathBuf>,
    env_vars: Vec<(String, String)>,
    non_interactive: bool,
    host_settings: Option<&HostSettings>,
    allow_docker: bool,
    runtime: &dyn crate::runtime::AgentRuntime,
) -> Result<()> {
    let git_root = find_git_root().context("Not inside a Git repository")?;
    let config = load_repo_config(&git_root)?;
    let agent = config.agent.as_deref().unwrap_or("claude").to_string();

    let _ = find_work_item(&git_root, work_item)?;

    let entrypoint = if non_interactive {
        amend_agent_entrypoint_non_interactive(&agent, work_item)
    } else {
        amend_agent_entrypoint(&agent, work_item)
    };

    let status = format!(
        "Amending work item {:04} with agent '{}'",
        work_item, agent
    );

    run_agent_with_sink(
        entrypoint,
        &status,
        out,
        mount_override,
        env_vars,
        non_interactive,
        host_settings,
        allow_docker,
        false,
        None,
        None,
        None,
        runtime,
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interview_prompt_contains_fields() {
        let prompt = interview_prompt(25, &WorkItemKind::Feature, "My Feature", "A brief summary");
        assert!(prompt.contains("0025"));
        assert!(prompt.contains("Feature"));
        assert!(prompt.contains("My Feature"));
        assert!(prompt.contains("A brief summary"));
    }

    #[test]
    fn amend_prompt_contains_number() {
        let prompt = amend_prompt(42);
        assert!(prompt.contains("0042"));
    }

    #[test]
    fn interview_agent_entrypoint_claude() {
        let ep = interview_agent_entrypoint("claude", 1, &WorkItemKind::Bug, "Fix it", "summary");
        assert_eq!(ep[0], "claude");
        assert!(ep[1].contains("0001"));
        assert!(ep[1].contains("Bug"));
        assert!(ep[1].contains("Fix it"));
        assert!(ep[1].contains("summary"));
    }

    #[test]
    fn interview_agent_entrypoint_codex() {
        let ep = interview_agent_entrypoint("codex", 2, &WorkItemKind::Task, "Do it", "details");
        assert_eq!(ep[0], "codex");
        assert!(ep[1].contains("0002"));
    }

    #[test]
    fn interview_agent_entrypoint_opencode() {
        let ep =
            interview_agent_entrypoint("opencode", 3, &WorkItemKind::Enhancement, "Enhance", "s");
        assert_eq!(ep[0], "opencode");
        assert_eq!(ep[1], "run");
        assert!(ep[2].contains("0003"));
    }

    #[test]
    fn amend_agent_entrypoint_claude() {
        let ep = amend_agent_entrypoint("claude", 10);
        assert_eq!(ep[0], "claude");
        assert!(ep[1].contains("0010"));
    }

    #[test]
    fn amend_agent_entrypoint_codex() {
        let ep = amend_agent_entrypoint("codex", 11);
        assert_eq!(ep[0], "codex");
        assert!(ep[1].contains("0011"));
    }

    #[test]
    fn amend_agent_entrypoint_opencode() {
        let ep = amend_agent_entrypoint("opencode", 12);
        assert_eq!(ep[0], "opencode");
        assert_eq!(ep[1], "run");
        assert!(ep[2].contains("0012"));
    }
}
