use crate::commands::agent::run_agent_with_sink;
use crate::commands::auth::resolve_auth;
use crate::commands::implement::confirm_mount_scope_stdin;
use crate::commands::init_flow::find_git_root_from;
use crate::commands::new_workflow::{validate_artefact_name, CONTAINER_WORKSPACE};
use crate::commands::output::OutputSink;
use crate::config::{global_skills_dir, load_repo_config};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Prompt template sent to the agent during `new skill --interview`.
pub const SKILL_INTERVIEW_PROMPT_TEMPLATE: &str = "A skill file has been created at {path}. \
Help complete the skill based on the following summary. The skill should include clear \
instructions that a code agent can follow step-by-step, with any relevant commands, code \
examples, or decision trees needed. Write the skill in the second person imperative \
(\"Run ...\", \"Check ...\", \"If ... then ...\"). Only edit the skill file at {path}. \
Do not create or edit any other files. Follow the YAML frontmatter already present in the \
skeleton. Do not summarize your work at the end — let the user review the file themselves.\n\n\
Summary:\n{summary}";

/// YAML frontmatter serialised via serde_yaml (guarantees correct quoting).
#[derive(Serialize)]
struct SkillFrontmatter<'a> {
    name: &'a str,
    description: &'a str,
}

/// Aggregated input for `new skill`.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillInput {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Resolve the destination directory for a skill (`<dir>/<name>/`).
///
/// - `global == true` writes to `~/.amux/skills/<name>/`.
/// - `global == false` writes to `<git_root>/.claude/skills/<name>/`.
///
/// Errors if `<dir>/SKILL.md` already exists.
pub fn resolve_skill_dest(
    name: &str,
    global: bool,
    git_root: Option<&Path>,
) -> Result<PathBuf> {
    let dir = if global {
        global_skills_dir()?.join(name)
    } else {
        let root = git_root.context(
            "Not inside a git repository. Use --global to write to ~/.amux/.",
        )?;
        root.join(".claude").join("skills").join(name)
    };
    let file = dir.join("SKILL.md");
    if file.exists() {
        bail!("Skill '{}' already exists at {}", name, file.display());
    }
    Ok(dir)
}

/// Render a skill file (YAML frontmatter + Markdown body).
pub fn render_skill_file(input: &SkillInput) -> String {
    let title = title_case(&input.name);
    let fm = SkillFrontmatter { name: &input.name, description: &input.description };
    let yaml = serde_yaml::to_string(&fm)
        .unwrap_or_else(|_| format!("name: {}\ndescription: {}\n", input.name, input.description));
    format!("---\n{}---\n\n# {}\n\n{}\n", yaml, title, input.body)
}

/// Render a skeleton skill file used in `--interview` mode.
pub fn render_skill_skeleton(name: &str, description: &str) -> String {
    let title = title_case(name);
    let fm = SkillFrontmatter { name, description };
    let yaml = serde_yaml::to_string(&fm)
        .unwrap_or_else(|_| format!("name: {}\ndescription: {}\n", name, description));
    format!(
        "---\n{}---\n\n# {}\n\n<!-- Agent will complete this file -->\n",
        yaml, title,
    )
}

/// Convert a kebab-case slug to Title Case for the heading.
fn title_case(name: &str) -> String {
    name.split(['-', '_'])
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Write a skill to disk at `<dir>/SKILL.md`.
pub fn write_skill_file(input: &SkillInput, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create directory {}", dir.display()))?;
    let path = dir.join("SKILL.md");
    let content = render_skill_file(input);
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

/// Write a skeleton skill (interview mode) to disk at `<dir>/SKILL.md`.
pub fn write_skill_skeleton(name: &str, description: &str, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create directory {}", dir.display()))?;
    let path = dir.join("SKILL.md");
    let content = render_skill_skeleton(name, description);
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

// ─── Interview prompt + entrypoint builders ───────────────────────────────────

pub fn skill_interview_prompt(path: &str, summary: &str) -> String {
    SKILL_INTERVIEW_PROMPT_TEMPLATE
        .replace("{path}", path)
        .replace("{summary}", summary)
}

pub fn skill_interview_agent_entrypoint(agent: &str, path: &str, summary: &str) -> Vec<String> {
    let prompt = skill_interview_prompt(path, summary);
    match agent {
        "claude" => vec!["claude".to_string(), prompt],
        "codex" => vec!["codex".to_string(), prompt],
        "opencode" => vec!["opencode".to_string(), "run".to_string(), prompt],
        _ => vec![agent.to_string(), prompt],
    }
}

pub fn skill_interview_agent_entrypoint_non_interactive(
    agent: &str,
    path: &str,
    summary: &str,
) -> Vec<String> {
    let prompt = skill_interview_prompt(path, summary);
    match agent {
        "claude" => vec!["claude".to_string(), "-p".to_string(), prompt],
        "codex" => vec!["codex".to_string(), "exec".to_string(), prompt],
        "opencode" => vec!["opencode".to_string(), "run".to_string(), prompt],
        _ => vec![agent.to_string(), prompt],
    }
}

// ─── CLI flow ─────────────────────────────────────────────────────────────────

/// Top-level CLI entry point for `amux new skill`.
pub async fn run_new_skill(interview: bool, global: bool) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let global_config = crate::config::load_global_config().unwrap_or_default();
    let runtime = crate::runtime::resolve_runtime(&global_config)?;
    run_new_skill_with_sink(
        &OutputSink::Stdout,
        &cwd,
        interview,
        global,
        None,
        None,
        None,
        None,
        &*runtime,
    )
    .await
}

/// Shared implementation used by both CLI and TUI.
#[allow(clippy::too_many_arguments)]
pub async fn run_new_skill_with_sink(
    out: &OutputSink,
    cwd: &Path,
    interview: bool,
    global: bool,
    name: Option<String>,
    description: Option<String>,
    body: Option<String>,
    summary: Option<String>,
    runtime: &dyn crate::runtime::AgentRuntime,
) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => prompt_line(out, "Skill name: ", true)?,
    };
    validate_artefact_name(&name)?;

    let description = match description {
        Some(d) => d,
        None => prompt_line(out, "Skill description (one line): ", true)?,
    };
    if description.is_empty() {
        bail!("Skill description cannot be empty.");
    }

    let git_root = find_git_root_from(cwd);
    if !global && git_root.is_none() {
        bail!("Not inside a git repository. Use --global to write to ~/.amux/.");
    }

    let dest_dir = resolve_skill_dest(&name, global, git_root.as_deref())?;

    if interview {
        let summary = match summary {
            Some(s) => s,
            None => prompt_line(out, "Enter a brief summary of this skill: ", true)?,
        };

        let path = write_skill_skeleton(&name, &description, &dest_dir)?;
        out.println(format!("Created skeleton skill: {}", path.display()));

        let git_root = git_root.context(
            "Not inside a git repository. The agent image requires a git repo with \
             `.amux/Dockerfile.<agent>`. Use --global without --interview to create without an agent.",
        )?;

        let (mount_path, container_path) = if global {
            let skill_dir = global_skills_dir()?.join(&name);
            std::fs::create_dir_all(&skill_dir)
                .with_context(|| format!("Failed to create directory {}", skill_dir.display()))?;
            (
                skill_dir,
                format!("{}/SKILL.md", CONTAINER_WORKSPACE),
            )
        } else {
            let mp = confirm_mount_scope_stdin(&git_root)?;
            let relative = path.strip_prefix(&mp).unwrap_or(path.as_path());
            let container_path = format!("{}/{}", CONTAINER_WORKSPACE, relative.to_string_lossy());
            (mp, container_path)
        };

        let agent = agent_name_from_config(&git_root)?;
        let credentials = resolve_auth(&git_root, &agent)?;
        let host_settings =
            crate::passthrough::passthrough_for_agent(&agent).prepare_host_settings();
        let entrypoint = skill_interview_agent_entrypoint(&agent, &container_path, &summary);

        let status = format!(
            "Running interview agent for skill '{}' with agent '{}'",
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

    let body = match body {
        Some(b) => b,
        None => read_multiline_until_period(
            out,
            "Enter skill body. End with a line containing only '.':",
        )?,
    };

    let input = SkillInput {
        name,
        description,
        body,
    };
    let path = write_skill_file(&input, &dest_dir)?;
    out.println(format!("Created skill: {}", path.display()));
    Ok(())
}

fn prompt_line(out: &OutputSink, prompt: &str, required: bool) -> Result<String> {
    out.print(prompt);
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;
    let value = input.trim().to_string();
    if required && value.is_empty() {
        bail!("Value cannot be empty.");
    }
    Ok(value)
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
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed == "." {
            break;
        }
        body.push_str(&line);
    }
    let body = body.trim_end_matches('\n').trim_end_matches('\r').to_string();
    if body.is_empty() {
        tracing::warn!("Skill body is empty. Continuing with empty body.");
    }
    Ok(body)
}

fn agent_name_from_config(git_root: &Path) -> Result<String> {
    let config = load_repo_config(git_root)?;
    Ok(config.agent.as_deref().unwrap_or("claude").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SkillInput {
        SkillInput {
            name: "my-skill".to_string(),
            description: "A test skill.".to_string(),
            body: "Run tests.".to_string(),
        }
    }

    #[test]
    fn render_skill_file_has_frontmatter_and_body() {
        let s = render_skill_file(&sample());
        assert!(s.starts_with("---\n"));
        assert!(s.contains("name: my-skill"));
        assert!(s.contains("description: A test skill."));
        assert!(s.contains("# My Skill"));
        assert!(s.contains("Run tests."));
    }

    #[test]
    fn render_skill_skeleton_has_placeholder() {
        let s = render_skill_skeleton("foo-bar", "do stuff");
        assert!(s.contains("name: foo-bar"));
        assert!(s.contains("# Foo Bar"));
        assert!(s.contains("Agent will complete"));
    }

    #[test]
    fn skill_interview_prompt_substitutes_fields() {
        let p = skill_interview_prompt("/workspace/SKILL.md", "do stuff");
        assert!(p.contains("/workspace/SKILL.md"));
        assert!(p.contains("do stuff"));
    }

    #[test]
    fn skill_interview_agent_entrypoint_claude() {
        let ep = skill_interview_agent_entrypoint("claude", "/workspace/SKILL.md", "summary");
        assert_eq!(ep[0], "claude");
    }

    #[test]
    fn skill_interview_agent_entrypoint_codex() {
        let ep = skill_interview_agent_entrypoint("codex", "/workspace/SKILL.md", "summary");
        assert_eq!(ep[0], "codex");
    }

    #[test]
    fn skill_interview_agent_entrypoint_opencode() {
        let ep =
            skill_interview_agent_entrypoint("opencode", "/workspace/SKILL.md", "summary");
        assert_eq!(ep[0], "opencode");
        assert_eq!(ep[1], "run");
    }

    // ── skill_interview_agent_entrypoint field substitution ───────────────────

    #[test]
    fn skill_interview_agent_entrypoint_claude_substitutes_path_and_summary() {
        let ep = skill_interview_agent_entrypoint(
            "claude",
            "/workspace/SKILL.md",
            "do stuff",
        );
        assert_eq!(ep[0], "claude");
        let prompt = &ep[1];
        assert!(prompt.contains("/workspace/SKILL.md"), "path must be substituted");
        assert!(prompt.contains("do stuff"), "summary must be substituted");
    }

    #[test]
    fn skill_interview_agent_entrypoint_codex_substitutes_path_and_summary() {
        let ep = skill_interview_agent_entrypoint(
            "codex",
            "/workspace/SKILL.md",
            "do stuff",
        );
        assert_eq!(ep[0], "codex");
        let prompt = &ep[1];
        assert!(prompt.contains("/workspace/SKILL.md"), "path must be substituted");
        assert!(prompt.contains("do stuff"), "summary must be substituted");
    }

    #[test]
    fn skill_interview_agent_entrypoint_opencode_substitutes_path_and_summary() {
        let ep = skill_interview_agent_entrypoint(
            "opencode",
            "/workspace/SKILL.md",
            "do stuff",
        );
        assert_eq!(ep[0], "opencode");
        assert_eq!(ep[1], "run");
        let prompt = &ep[2];
        assert!(prompt.contains("/workspace/SKILL.md"), "path must be substituted");
        assert!(prompt.contains("do stuff"), "summary must be substituted");
    }

    // ── write_skill_file ──────────────────────────────────────────────────────

    #[test]
    fn write_skill_file_creates_skill_md_with_correct_frontmatter_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let input = SkillInput {
            name: "my-skill".to_string(),
            description: "A test skill.".to_string(),
            body: "Run the tests.".to_string(),
        };
        let path = write_skill_file(&input, dir.path()).unwrap();
        assert_eq!(path, dir.path().join("SKILL.md"), "file must be named SKILL.md");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("---\n"), "must start with YAML frontmatter delimiter");
        assert!(content.contains("name: my-skill"), "frontmatter must contain name");
        assert!(content.contains("description: A test skill."), "frontmatter must contain description");
        assert!(content.contains("---\n"), "frontmatter delimiter must be closed");
        assert!(content.contains("Run the tests."), "body must be present");
    }

    // ── skeleton frontmatter and placeholder ──────────────────────────────────

    #[test]
    fn write_skill_skeleton_creates_skill_md_with_frontmatter_and_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill_skeleton("foo-skill", "Do foo things.", dir.path()).unwrap();
        assert_eq!(path, dir.path().join("SKILL.md"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("---\n"), "skeleton must start with YAML frontmatter");
        assert!(content.contains("name: foo-skill"), "skeleton must have name in frontmatter");
        assert!(content.contains("description: Do foo things."), "skeleton must have description");
        assert!(content.contains("Agent will complete"), "skeleton must contain placeholder body");
        // Must NOT have a real body substituted.
        assert!(!content.contains("Run the tests."), "skeleton must not have real body");
    }

    // ── local container path ──────────────────────────────────────────────────

    #[test]
    fn local_skill_container_path_is_workspace_relative() {
        let dir = tempfile::tempdir().unwrap();
        let dest_dir = dir.path().join(".claude").join("skills").join("my-skill");
        let path = dest_dir.join("SKILL.md");
        let mp = dir.path().to_path_buf();
        let relative = path.strip_prefix(&mp).unwrap_or(path.as_path());
        let container_path = format!("{}/{}", CONTAINER_WORKSPACE, relative.to_string_lossy());
        assert_eq!(container_path, "/workspace/.claude/skills/my-skill/SKILL.md");
    }

    // ── global mount path ─────────────────────────────────────────────────────

    #[test]
    fn global_skill_mount_path_for_named_skill_equals_global_skills_dir_slash_name() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AMUX_CONFIG_HOME", dir.path()) };
        let skills_dir = crate::config::global_skills_dir();
        unsafe { std::env::remove_var("AMUX_CONFIG_HOME") };
        let mount = skills_dir.unwrap().join("foo");
        assert_eq!(mount, dir.path().join("skills").join("foo"));
    }
}
