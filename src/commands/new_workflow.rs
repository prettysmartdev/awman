use crate::cli::WorkflowFormat;
use crate::commands::agent::run_agent_with_sink;
use crate::commands::auth::resolve_auth;
use crate::commands::implement::confirm_mount_scope_stdin;
use crate::commands::init_flow::find_git_root_from;
use crate::commands::output::OutputSink;
use crate::config::{global_workflows_dir, load_repo_config};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Container path used when mounting a non-repo directory into the agent container.
pub const CONTAINER_WORKSPACE: &str = "/workspace";

/// Prompt template sent to the agent during `new workflow --interview`.
pub const WORKFLOW_INTERVIEW_PROMPT_TEMPLATE: &str = "Workflow file {filename} has been created at \
{path}. Help complete the workflow based on the following summary. The workflow should include all \
necessary steps with clear step names, explicit depends_on relationships, appropriate agent and \
model choices where relevant, and detailed, actionable prompts for each step. Only edit the \
workflow file. Do not create or edit any other files. Follow the file format already present in \
the skeleton. Do not summarize your work at the end — let the user review the file themselves.\n\n\
Summary:\n{summary}";

/// A single step in a workflow being constructed.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowStepInput {
    pub name: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub depends_on: Vec<String>,
    pub prompt: String,
}

/// Aggregated input for `new workflow`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowInput {
    pub title: String,
    pub steps: Vec<WorkflowStepInput>,
}

// ─── Serde structs for TOML / YAML output ─────────────────────────────────────

#[derive(Debug, Serialize)]
struct WorkflowFile<'a> {
    title: &'a str,
    #[serde(rename = "step", skip_serializing_if = "Vec::is_empty")]
    steps_toml: Vec<WorkflowStepFile<'a>>,
}

#[derive(Debug, Serialize)]
struct WorkflowFileYaml<'a> {
    title: &'a str,
    steps: Vec<WorkflowStepFile<'a>>,
}

#[derive(Debug, Serialize)]
struct WorkflowStepFile<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    prompt: &'a str,
}

// ─── Validation ───────────────────────────────────────────────────────────────

/// Validate a workflow / skill name. Names must be non-empty, contain only
/// alphanumeric characters, hyphens, and underscores, and must not contain
/// path separators.
pub fn validate_artefact_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Name cannot be empty.");
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!(
                "Invalid name '{}': only alphanumeric characters, hyphens, and underscores are allowed.",
                name
            );
        }
    }
    Ok(())
}

// ─── Path resolution ──────────────────────────────────────────────────────────

/// Resolve the destination path for a workflow file.
///
/// - `global == true` writes to `~/.amux/workflows/<name>.<ext>`.
/// - `global == false` writes to `<git_root>/aspec/workflows/<name>.<ext>`.
///
/// Errors if the destination already exists.
pub fn resolve_workflow_dest(
    name: &str,
    global: bool,
    format: &WorkflowFormat,
    git_root: Option<&Path>,
) -> Result<PathBuf> {
    let filename = format!("{}.{}", name, format.extension());
    let dest = if global {
        global_workflows_dir()?.join(&filename)
    } else {
        let root = git_root.context(
            "Not inside a git repository. Use --global to write to ~/.amux/.",
        )?;
        let dir = root.join("aspec").join("workflows");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create directory {}", dir.display()))?;
        dir.join(&filename)
    };
    if dest.exists() {
        bail!("Workflow '{}' already exists at {}", name, dest.display());
    }
    Ok(dest)
}

// ─── Serialisation ────────────────────────────────────────────────────────────

/// Serialise a `WorkflowInput` to disk in the requested format.
pub fn write_workflow_file(
    input: &WorkflowInput,
    dest: &Path,
    format: &WorkflowFormat,
) -> Result<()> {
    let content = serialize_workflow(input, format)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    std::fs::write(dest, content)
        .with_context(|| format!("Failed to write {}", dest.display()))
}

/// Render a `WorkflowInput` into a string in the requested format.
pub fn serialize_workflow(input: &WorkflowInput, format: &WorkflowFormat) -> Result<String> {
    match format {
        WorkflowFormat::Toml => serialize_workflow_toml(input),
        WorkflowFormat::Yaml => serialize_workflow_yaml(input),
        WorkflowFormat::Md => Ok(serialize_workflow_md(input)),
    }
}

fn step_files(input: &WorkflowInput) -> Vec<WorkflowStepFile<'_>> {
    input
        .steps
        .iter()
        .map(|s| WorkflowStepFile {
            name: &s.name,
            depends_on: s.depends_on.iter().map(String::as_str).collect(),
            agent: s.agent.as_deref(),
            model: s.model.as_deref(),
            prompt: &s.prompt,
        })
        .collect()
}

fn serialize_workflow_toml(input: &WorkflowInput) -> Result<String> {
    let file = WorkflowFile {
        title: &input.title,
        steps_toml: step_files(input),
    };
    toml::to_string_pretty(&file).context("Failed to serialise workflow as TOML")
}

fn serialize_workflow_yaml(input: &WorkflowInput) -> Result<String> {
    let file = WorkflowFileYaml {
        title: &input.title,
        steps: step_files(input),
    };
    serde_yaml::to_string(&file).context("Failed to serialise workflow as YAML")
}

fn serialize_workflow_md(input: &WorkflowInput) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n", input.title));
    for step in &input.steps {
        out.push_str(&format!("\n## Step: {}\n", step.name));
        if !step.depends_on.is_empty() {
            out.push_str(&format!("Depends-on: {}\n", step.depends_on.join(", ")));
        }
        if let Some(agent) = &step.agent {
            out.push_str(&format!("Agent: {}\n", agent));
        }
        if let Some(model) = &step.model {
            out.push_str(&format!("Model: {}\n", model));
        }
        out.push_str(&format!("Prompt: {}\n", step.prompt));
    }
    out
}

/// Build a skeleton workflow file (title only, no steps) used in `--interview` mode.
///
/// Serialises through the same serde structs used for the full workflow so that
/// special characters in `title` are always correctly escaped.
pub fn skeleton_workflow(title: &str, format: &WorkflowFormat) -> String {
    match format {
        WorkflowFormat::Toml => {
            let file = WorkflowFile { title, steps_toml: vec![] };
            toml::to_string_pretty(&file)
                .unwrap_or_else(|_| format!("title = \"{}\"\n", title.replace('"', "\\\"")))
        }
        WorkflowFormat::Yaml => {
            let file = WorkflowFileYaml { title, steps: vec![] };
            serde_yaml::to_string(&file)
                .unwrap_or_else(|_| format!("title: \"{}\"\nsteps: []\n", title.replace('"', "\\\"")))
        }
        WorkflowFormat::Md => format!("# {}\n", title),
    }
}

// ─── Interview prompt + entrypoint builders ───────────────────────────────────

/// Build the interview prompt for `new workflow --interview`.
pub fn workflow_interview_prompt(filename: &str, path: &str, summary: &str) -> String {
    WORKFLOW_INTERVIEW_PROMPT_TEMPLATE
        .replace("{filename}", filename)
        .replace("{path}", path)
        .replace("{summary}", summary)
}

/// Interactive agent entrypoint for the workflow interview.
pub fn workflow_interview_agent_entrypoint(
    agent: &str,
    path: &str,
    filename: &str,
    summary: &str,
) -> Vec<String> {
    let prompt = workflow_interview_prompt(filename, path, summary);
    match agent {
        "claude" => vec!["claude".to_string(), prompt],
        "codex" => vec!["codex".to_string(), prompt],
        "opencode" => vec!["opencode".to_string(), "run".to_string(), prompt],
        _ => vec![agent.to_string(), prompt],
    }
}

/// Non-interactive agent entrypoint for the workflow interview.
pub fn workflow_interview_agent_entrypoint_non_interactive(
    agent: &str,
    path: &str,
    filename: &str,
    summary: &str,
) -> Vec<String> {
    let prompt = workflow_interview_prompt(filename, path, summary);
    match agent {
        "claude" => vec!["claude".to_string(), "-p".to_string(), prompt],
        "codex" => vec!["codex".to_string(), "exec".to_string(), prompt],
        "opencode" => vec!["opencode".to_string(), "run".to_string(), prompt],
        _ => vec![agent.to_string(), prompt],
    }
}

// ─── CLI flow ─────────────────────────────────────────────────────────────────

/// Top-level CLI entry point for `amux new workflow`.
pub async fn run_new_workflow(
    interview: bool,
    global: bool,
    format: WorkflowFormat,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let global_config = crate::config::load_global_config().unwrap_or_default();
    let runtime = crate::runtime::resolve_runtime(&global_config)?;
    run_new_workflow_with_sink(
        &OutputSink::Stdout,
        &cwd,
        interview,
        global,
        format,
        None,
        None,
        None,
        &*runtime,
    )
    .await
}

/// Shared implementation used by both CLI and TUI.
///
/// `name`, `title`, and `summary` may be pre-supplied (TUI) or `None` to prompt
/// interactively over stdin.
#[allow(clippy::too_many_arguments)]
pub async fn run_new_workflow_with_sink(
    out: &OutputSink,
    cwd: &Path,
    interview: bool,
    global: bool,
    format: WorkflowFormat,
    name: Option<String>,
    title: Option<String>,
    summary: Option<String>,
    runtime: &dyn crate::runtime::AgentRuntime,
) -> Result<()> {
    // Workflow name (used as filename and slug).
    let name = match name {
        Some(n) => n,
        None => prompt_workflow_name(out)?,
    };
    validate_artefact_name(&name)?;

    let git_root = find_git_root_from(cwd);
    if !global && git_root.is_none() {
        bail!("Not inside a git repository. Use --global to write to ~/.amux/.");
    }

    let dest = resolve_workflow_dest(&name, global, &format, git_root.as_deref())?;
    let filename = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    if interview {
        // For interview mode, prompt for a one-line summary, write a skeleton, and launch
        // the agent.
        let title_value = match title {
            Some(t) => t,
            None => name.clone(),
        };
        let summary = match summary {
            Some(s) => s,
            None => prompt_summary(out, "Enter a brief summary of this workflow: ")?,
        };

        let skeleton = skeleton_workflow(&title_value, &format);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }
        std::fs::write(&dest, &skeleton)
            .with_context(|| format!("Failed to write {}", dest.display()))?;
        out.println(format!("Created skeleton workflow: {}", dest.display()));

        // --interview always requires a git repo (for agent-image lookup).
        let git_root = git_root.context(
            "Not inside a git repository. The agent image requires a git repo with \
             `.amux/Dockerfile.<agent>`. Use --global without --interview to create without an agent.",
        )?;

        // Determine mount path. For --global we mount the global workflows dir; otherwise
        // ask the user to confirm Git root vs CWD.
        let (mount_path, container_path) = if global {
            let wf_dir = global_workflows_dir()?;
            let container_path =
                format!("{}/{}", CONTAINER_WORKSPACE, filename);
            (wf_dir, container_path)
        } else {
            let mp = confirm_mount_scope_stdin(&git_root)?;
            let relative = dest.strip_prefix(&mp).unwrap_or(dest.as_path());
            let container_path = format!("{}/{}", CONTAINER_WORKSPACE, relative.to_string_lossy());
            (mp, container_path)
        };

        let agent = agent_name_from_config(&git_root)?;
        let credentials = resolve_auth(&git_root, &agent)?;
        let host_settings =
            crate::passthrough::passthrough_for_agent(&agent).prepare_host_settings();
        let entrypoint = workflow_interview_agent_entrypoint(
            &agent,
            &container_path,
            &filename,
            &summary,
        );

        let status = format!(
            "Running interview agent for workflow '{}' with agent '{}'",
            name, agent
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

        return Ok(());
    }

    // Non-interview: collect title, then loop steps.
    let title = match title {
        Some(t) => t,
        None => prompt_workflow_title(out)?,
    };

    let steps = collect_workflow_steps_stdin(out)?;
    if steps.is_empty() {
        bail!("At least one step is required.");
    }

    let input = WorkflowInput { title, steps };
    write_workflow_file(&input, &dest, &format)?;
    out.println(format!("Created workflow: {}", dest.display()));
    Ok(())
}

// ─── Stdin prompts ────────────────────────────────────────────────────────────

fn prompt_workflow_name(out: &OutputSink) -> Result<String> {
    out.print("Workflow name: ");
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;
    let name = input.trim().to_string();
    if name.is_empty() {
        bail!("Workflow name cannot be empty.");
    }
    Ok(name)
}

fn prompt_workflow_title(out: &OutputSink) -> Result<String> {
    out.print("Workflow title (human-readable): ");
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;
    let title = input.trim().to_string();
    if title.is_empty() {
        bail!("Workflow title cannot be empty.");
    }
    Ok(title)
}

fn prompt_summary(out: &OutputSink, prompt: &str) -> Result<String> {
    out.print(prompt);
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

fn prompt_line(out: &OutputSink, prompt: &str) -> Result<String> {
    out.print(prompt);
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;
    Ok(input.trim().to_string())
}

fn read_multiline_until_period(out: &OutputSink, prompt: &str) -> Result<String> {
    out.println(prompt);
    let mut body = String::new();
    let stdin = std::io::stdin();
    loop {
        let mut line = String::new();
        let bytes = stdin.read_line(&mut line).context("Failed to read input")?;
        if bytes == 0 {
            break;
        }
        let trimmed_no_newline = line.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed_no_newline == "." {
            break;
        }
        body.push_str(&line);
    }
    let body = body.trim_end_matches('\n').trim_end_matches('\r').to_string();
    if body.is_empty() {
        tracing::warn!("Prompt is empty. Continuing with empty prompt.");
    }
    Ok(body)
}

fn collect_workflow_steps_stdin(out: &OutputSink) -> Result<Vec<WorkflowStepInput>> {
    let mut steps: Vec<WorkflowStepInput> = Vec::new();
    loop {
        let name = prompt_line(out, "Step name: ")?;
        if name.is_empty() {
            bail!("Step name cannot be empty.");
        }
        let agent = {
            let s = prompt_line(out, "Agent (optional, press Enter to skip): ")?;
            if s.is_empty() { None } else { Some(s) }
        };
        let model = {
            let s = prompt_line(out, "Model (optional, press Enter to skip): ")?;
            if s.is_empty() { None } else { Some(s) }
        };
        let depends_on_raw = prompt_line(
            out,
            "Depends-on (optional, comma-separated step names, press Enter to skip): ",
        )?;
        let depends_on: Vec<String> = depends_on_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let prompt = read_multiline_until_period(
            out,
            "Enter prompt text. End with a line containing only '.':",
        )?;

        steps.push(WorkflowStepInput {
            name,
            agent,
            model,
            depends_on,
            prompt,
        });

        let again = prompt_line(out, "Add another step? [y/N]: ")?;
        if !matches!(again.trim().to_lowercase().as_str(), "y" | "yes") {
            break;
        }
    }
    Ok(steps)
}

fn agent_name_from_config(git_root: &Path) -> Result<String> {
    let config = load_repo_config(git_root)?;
    Ok(config.agent.as_deref().unwrap_or("claude").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input() -> WorkflowInput {
        WorkflowInput {
            title: "Demo".to_string(),
            steps: vec![
                WorkflowStepInput {
                    name: "plan".to_string(),
                    agent: None,
                    model: None,
                    depends_on: vec![],
                    prompt: "Plan the work.".to_string(),
                },
                WorkflowStepInput {
                    name: "implement".to_string(),
                    agent: Some("codex".to_string()),
                    model: Some("claude-opus-4-7".to_string()),
                    depends_on: vec!["plan".to_string()],
                    prompt: "Implement the plan.".to_string(),
                },
            ],
        }
    }

    #[test]
    fn validate_artefact_name_accepts_simple_kebab() {
        validate_artefact_name("my-workflow").unwrap();
    }

    #[test]
    fn validate_artefact_name_rejects_empty() {
        assert!(validate_artefact_name("").is_err());
    }

    #[test]
    fn validate_artefact_name_rejects_spaces() {
        assert!(validate_artefact_name("my workflow").is_err());
    }

    #[test]
    fn validate_artefact_name_rejects_path_separator() {
        assert!(validate_artefact_name("foo/bar").is_err());
        assert!(validate_artefact_name("foo\\bar").is_err());
    }

    #[test]
    fn serialize_toml_roundtrips_through_parser() {
        let input = sample_input();
        let toml_str = serialize_workflow(&input, &WorkflowFormat::Toml).unwrap();
        let (title, steps) = crate::workflow::parser::parse_workflow_toml(&toml_str).unwrap();
        assert_eq!(title.as_deref(), Some("Demo"));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].name, "plan");
        assert_eq!(steps[1].name, "implement");
        assert_eq!(steps[1].depends_on, vec!["plan"]);
        assert_eq!(steps[1].agent.as_deref(), Some("codex"));
        assert_eq!(steps[1].model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn serialize_yaml_roundtrips_through_parser() {
        let input = sample_input();
        let yaml_str = serialize_workflow(&input, &WorkflowFormat::Yaml).unwrap();
        let (title, steps) = crate::workflow::parser::parse_workflow_yaml(&yaml_str).unwrap();
        assert_eq!(title.as_deref(), Some("Demo"));
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn serialize_md_roundtrips_through_parser() {
        let input = sample_input();
        let md_str = serialize_workflow(&input, &WorkflowFormat::Md).unwrap();
        let (title, steps) = crate::workflow::parser::parse_workflow(&md_str).unwrap();
        assert_eq!(title.as_deref(), Some("Demo"));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].depends_on, vec!["plan"]);
    }

    #[test]
    fn workflow_interview_prompt_substitutes_fields() {
        let p = workflow_interview_prompt("foo.toml", "/workspace/foo.toml", "do stuff");
        assert!(p.contains("foo.toml"));
        assert!(p.contains("/workspace/foo.toml"));
        assert!(p.contains("do stuff"));
    }

    #[test]
    fn workflow_interview_agent_entrypoint_claude() {
        let ep =
            workflow_interview_agent_entrypoint("claude", "/workspace/foo.toml", "foo.toml", "s");
        assert_eq!(ep[0], "claude");
        assert!(ep[1].contains("foo.toml"));
    }

    #[test]
    fn workflow_interview_agent_entrypoint_opencode() {
        let ep = workflow_interview_agent_entrypoint(
            "opencode",
            "/workspace/foo.toml",
            "foo.toml",
            "s",
        );
        assert_eq!(ep[0], "opencode");
        assert_eq!(ep[1], "run");
    }

    // ── write_workflow_file ───────────────────────────────────────────────────

    #[test]
    fn write_workflow_file_toml_creates_file_with_correct_content() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("my-workflow.toml");
        write_workflow_file(&sample_input(), &dest, &WorkflowFormat::Toml).unwrap();
        assert!(dest.exists());
        let content = std::fs::read_to_string(&dest).unwrap();
        assert!(content.contains("Demo"), "title must appear");
        assert!(content.contains("plan"), "first step name must appear");
        assert!(content.contains("implement"), "second step name must appear");
        assert!(content.contains("codex"), "optional agent must appear");
        assert!(content.contains("claude-opus-4-7"), "optional model must appear");
    }

    #[test]
    fn write_workflow_file_yaml_creates_file_with_correct_content() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("my-workflow.yaml");
        write_workflow_file(&sample_input(), &dest, &WorkflowFormat::Yaml).unwrap();
        assert!(dest.exists());
        let content = std::fs::read_to_string(&dest).unwrap();
        assert!(content.contains("Demo"));
        assert!(content.contains("plan"));
        assert!(content.contains("implement"));
    }

    #[test]
    fn write_workflow_file_md_creates_file_with_correct_content() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("my-workflow.md");
        write_workflow_file(&sample_input(), &dest, &WorkflowFormat::Md).unwrap();
        assert!(dest.exists());
        let content = std::fs::read_to_string(&dest).unwrap();
        assert!(content.starts_with("# Demo"), "Markdown must start with title heading");
        assert!(content.contains("## Step: plan"), "step heading must appear");
        assert!(content.contains("## Step: implement"), "second step heading must appear");
    }

    #[test]
    fn serialize_toml_omits_optional_fields_when_absent() {
        let input = WorkflowInput {
            title: "Minimal".to_string(),
            steps: vec![WorkflowStepInput {
                name: "only-step".to_string(),
                agent: None,
                model: None,
                depends_on: vec![],
                prompt: "Do it.".to_string(),
            }],
        };
        let s = serialize_workflow(&input, &WorkflowFormat::Toml).unwrap();
        assert!(!s.contains("agent"), "agent must be absent when None; got:\n{}", s);
        assert!(!s.contains("model"), "model must be absent when None; got:\n{}", s);
        assert!(!s.contains("depends_on"), "depends_on must be absent when empty; got:\n{}", s);
    }

    #[test]
    fn serialize_yaml_omits_optional_fields_when_absent() {
        let input = WorkflowInput {
            title: "Minimal".to_string(),
            steps: vec![WorkflowStepInput {
                name: "only-step".to_string(),
                agent: None,
                model: None,
                depends_on: vec![],
                prompt: "Do it.".to_string(),
            }],
        };
        let s = serialize_workflow(&input, &WorkflowFormat::Yaml).unwrap();
        assert!(!s.contains("agent:"), "agent must be absent when None; got:\n{}", s);
        assert!(!s.contains("model:"), "model must be absent when None; got:\n{}", s);
        assert!(!s.contains("depends_on:"), "depends_on must be absent when empty; got:\n{}", s);
    }

    // ── resolve_workflow_dest ─────────────────────────────────────────────────

    #[test]
    fn resolve_workflow_dest_local_uses_git_root_aspec_workflows() {
        let dir = tempfile::tempdir().unwrap();
        let dest = resolve_workflow_dest(
            "my-wf",
            false,
            &WorkflowFormat::Toml,
            Some(dir.path()),
        )
        .unwrap();
        assert_eq!(
            dest,
            dir.path().join("aspec").join("workflows").join("my-wf.toml")
        );
    }

    #[test]
    fn resolve_workflow_dest_local_yaml_extension() {
        let dir = tempfile::tempdir().unwrap();
        let dest = resolve_workflow_dest(
            "my-wf",
            false,
            &WorkflowFormat::Yaml,
            Some(dir.path()),
        )
        .unwrap();
        assert_eq!(
            dest,
            dir.path().join("aspec").join("workflows").join("my-wf.yaml")
        );
    }

    #[test]
    fn resolve_workflow_dest_global_uses_amux_config_home() {
        let dir = tempfile::tempdir().unwrap();
        // AMUX_CONFIG_HOME redirects global_workflows_dir() to a temp path.
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", dir.path()) };
        let result = resolve_workflow_dest("my-wf", true, &WorkflowFormat::Toml, None);
        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        let dest = result.unwrap();
        assert_eq!(dest, dir.path().join("workflows").join("my-wf.toml"));
    }

    #[test]
    fn resolve_workflow_dest_local_without_git_root_errors() {
        let err = resolve_workflow_dest("my-wf", false, &WorkflowFormat::Toml, None).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("git"),
            "error must mention git; got: {}",
            err
        );
    }

    // ── agent entrypoint field substitution ───────────────────────────────────

    #[test]
    fn workflow_interview_agent_entrypoint_claude_substitutes_all_fields() {
        let ep = workflow_interview_agent_entrypoint(
            "claude",
            "/workspace/foo.toml",
            "foo.toml",
            "do stuff",
        );
        assert_eq!(ep[0], "claude");
        let prompt = &ep[1];
        assert!(prompt.contains("/workspace/foo.toml"), "path must be substituted");
        assert!(prompt.contains("foo.toml"), "filename must be substituted");
        assert!(prompt.contains("do stuff"), "summary must be substituted");
    }

    #[test]
    fn workflow_interview_agent_entrypoint_codex_substitutes_all_fields() {
        let ep = workflow_interview_agent_entrypoint(
            "codex",
            "/workspace/foo.toml",
            "foo.toml",
            "do stuff",
        );
        assert_eq!(ep[0], "codex");
        let prompt = &ep[1];
        assert!(prompt.contains("/workspace/foo.toml"), "path must be substituted");
        assert!(prompt.contains("foo.toml"), "filename must be substituted");
        assert!(prompt.contains("do stuff"), "summary must be substituted");
    }

    #[test]
    fn workflow_interview_agent_entrypoint_opencode_substitutes_all_fields() {
        let ep = workflow_interview_agent_entrypoint(
            "opencode",
            "/workspace/foo.toml",
            "foo.toml",
            "do stuff",
        );
        assert_eq!(ep[0], "opencode");
        assert_eq!(ep[1], "run");
        let prompt = &ep[2];
        assert!(prompt.contains("/workspace/foo.toml"), "path must be substituted");
        assert!(prompt.contains("foo.toml"), "filename must be substituted");
        assert!(prompt.contains("do stuff"), "summary must be substituted");
    }

    // ── local container path ──────────────────────────────────────────────────

    #[test]
    fn local_workflow_container_path_is_workspace_relative() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("aspec").join("workflows").join("my-wf.toml");
        let mp = dir.path().to_path_buf();
        let relative = dest.strip_prefix(&mp).unwrap_or(dest.as_path());
        let container_path = format!("{}/{}", CONTAINER_WORKSPACE, relative.to_string_lossy());
        assert_eq!(container_path, "/workspace/aspec/workflows/my-wf.toml");
    }

    // ── global mount path ─────────────────────────────────────────────────────

    #[test]
    fn global_workflow_mount_path_equals_global_workflows_dir() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", dir.path()) };
        let wf_dir = crate::config::global_workflows_dir();
        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        let wf_dir = wf_dir.unwrap();
        assert_eq!(wf_dir, dir.path().join("workflows"));
    }
}
