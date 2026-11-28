//! Embedded static assets for dynamic workflows (`exec workflow --dynamic`).
//!
//! These are compiled into the binary from `src/assets/dynamic/` and are
//! always regenerated from this embedded content at runtime — never read from
//! the host filesystem. See WI-0092.
//!
//! - [`WORKFLOW_USAGE_MD`] is written into the leader's workflow context
//!   directory as reference material, including several inline examples.
//! - [`LEADER_PROMPT_MD`] is the leader prompt template; it is substituted in
//!   code (never written to disk) before being delivered to the leader agent.
//! - [`LEADER_REPAIR_PROMPT`] is the repair prompt template used when the
//!   leader's `workflow.toml` fails validation.

/// The complete workflow file-format documentation. Written to
/// `<context_dir>/workflow-usage.md`.
pub const WORKFLOW_USAGE_MD: &str = include_str!("../assets/dynamic/workflow-usage.md");

/// The leader prompt template. Substituted with `{{work_item_identity}}`,
/// `{{work_item_path}}`, `{{available_agents}}`, `{{max_concurrent_steps}}`
/// and `{{developer_guidance}}` before being delivered.
pub const LEADER_PROMPT_MD: &str = include_str!("../assets/dynamic/leader-prompt.md");

/// The repair prompt template. Substituted with `{{validation_error}}`.
pub const LEADER_REPAIR_PROMPT: &str = include_str!("../assets/dynamic/leader-repair-prompt.md");

/// The task-evaluation leader prompt. Unlike a dynamic work-item leader,
/// this agent first reports whether the task is met.
pub const SQUAD_LEADER_PROMPT_MD: &str = include_str!("../assets/dynamic/squad-leader-prompt.md");

/// The task's overlay inventory, as five ready-to-substitute lists.
///
/// Data only: each field is a bullet list, or `(none)`. Every heading and
/// sentence around them lives in `squad-leader-prompt.md`, so the prompt reads
/// and edits as prose in one place. Layer 2 fills this in; see
/// `command::commands::squad::overlay_summary` (WI 0117).
///
/// A leader not told what its containers actually have either assumes too much
/// and writes steps that fail at runtime, or assumes too little and writes a
/// workflow weaker than the task allows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayInventory {
    /// Host directories, as `container path (permission) — from host path`.
    pub directories: String,
    /// `env()` names the daemon holds a value for.
    pub env_set: String,
    /// `env()` names the task declares that the daemon has no value for.
    pub env_unset: String,
    /// Skills mounted into the agent's skills directory.
    pub skills: String,
    /// `context(global)` / `context(repo)` directories, which reach the
    /// generated workflow's steps but not the leader's own container.
    pub context: String,
}

/// The image-recipe section of a directory task's leader prompt (WI 0105).
pub const SQUAD_LEADER_IMAGE_RECIPES_MD: &str =
    include_str!("../assets/dynamic/squad-leader-image-recipes.md");

/// The task-owned image recipes a directory task's leader is told about,
/// as container paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRecipePrompt {
    /// `/awman/context/workflow/Dockerfile.dev`.
    pub project: String,
    /// `(agent, /awman/context/workflow/.awman/Dockerfile.<agent>)`.
    pub agents: Vec<(String, String)>,
    /// A default task: `/workspace` and `/awman/context/workflow` are one
    /// directory.
    pub same_directory: bool,
}

impl ImageRecipePrompt {
    /// The recipes under the task context mount `context_mount`.
    pub fn at(context_mount: &str, agents: &[String], same_directory: bool) -> Self {
        Self {
            project: format!("{context_mount}/Dockerfile.dev"),
            agents: agents
                .iter()
                .map(|agent| {
                    (
                        agent.clone(),
                        format!("{context_mount}/.awman/Dockerfile.{agent}"),
                    )
                })
                .collect(),
            same_directory,
        }
    }

    fn render(&self, repo_mount_path: &str) -> String {
        let agents = if self.agents.is_empty() {
            "- (no agent recipes are prepared for this task)".to_string()
        } else {
            self.agents
                .iter()
                .map(|(agent, path)| format!("- `{path}` — the `{agent}` agent image."))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let note = if self.same_directory {
            format!(
                "\nFor this task `{repo_mount_path}` and `/awman/context/workflow` are the same \
                 directory, so the recipes are also visible under `{repo_mount_path}`; editing \
                 them still only changes the images of the next run.\n"
            )
        } else {
            String::new()
        };
        SQUAD_LEADER_IMAGE_RECIPES_MD
            .replace("{{project_recipe}}", &self.project)
            .replace("{{agent_recipes}}", &agents)
            .replace("{{repo_mount_path}}", repo_mount_path)
            .replace("{{same_directory_note}}", &note)
    }
}

/// Construct the squad evaluation-leader prompt. `image_recipes` is `Some`
/// only for a directory task, whose images are built from task-owned
/// recipes; a Git-backed task's prompt has no recipe section.
#[allow(clippy::too_many_arguments)]
pub fn build_squad_leader_prompt(
    task_name: &str,
    task_description: &str,
    repo_mount_path: &str,
    available_agents: &str,
    overlays: &OverlayInventory,
    verdict_path: &str,
    guidance: Option<&[String]>,
    image_recipes: Option<&ImageRecipePrompt>,
) -> String {
    let recipes = image_recipes
        .map(|recipes| format!("\n{}", recipes.render(repo_mount_path)))
        .unwrap_or_default();
    // Substituted first, so only awman's own text — never a task description
    // that happens to contain the placeholder — becomes the section.
    SQUAD_LEADER_PROMPT_MD
        .replace("{{image_recipes}}\n", &recipes)
        .replace("{{task_name}}", task_name)
        .replace("{{task_description}}", task_description)
        .replace("{{repo_mount_path}}", repo_mount_path)
        .replace("{{available_agents}}", available_agents)
        .replace("{{overlay_directories}}", &overlays.directories)
        .replace("{{overlay_env_set}}", &overlays.env_set)
        .replace("{{overlay_env_unset}}", &overlays.env_unset)
        .replace("{{overlay_skills}}", &overlays.skills)
        .replace("{{overlay_context}}", &overlays.context)
        .replace("{{verdict_path}}", verdict_path)
        .replace(
            "{{developer_guidance}}",
            &build_developer_guidance(guidance),
        )
}

/// Construct the leader prompt by substituting the runtime template variables
/// into [`LEADER_PROMPT_MD`].
pub fn build_leader_prompt(
    work_item_identity: &str,
    work_item_path: &str,
    available_agents: &str,
    max_concurrent_steps: Option<usize>,
    guidance: Option<&[String]>,
) -> String {
    let max_concurrent_steps = match max_concurrent_steps {
        Some(n) => n.to_string(),
        None => "no limit".to_string(),
    };
    let developer_guidance = build_developer_guidance(guidance);
    // Substitute only the original template: inserted text is prompt data,
    // even when it contains characters that look like another placeholder.
    let values = [
        ("work_item_identity", work_item_identity),
        ("work_item_path", work_item_path),
        ("available_agents", available_agents),
        ("max_concurrent_steps", max_concurrent_steps.as_str()),
        ("developer_guidance", developer_guidance.as_str()),
    ];
    let mut out = String::new();
    let mut rest = LEADER_PROMPT_MD;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let end = start + end + 2;
        let name = &rest[start + 2..end - 2];
        match values.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[start..end]),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Render resolved `workflows.dynamic.guidance` entries as the bullet
/// list both leader prompts substitute into their Developer Guidance section
/// (WI-0099).
///
/// Data only: the heading and lead-in live in the templates. Absence renders
/// `(none)` rather than an empty string, so the templates' spacing stays
/// static and a reader can tell "nothing configured" from "section missing".
///
/// Whitespace-only entries are skipped and any literal newlines within an entry
/// are flattened to spaces, so each entry stays a single bullet point.
fn build_developer_guidance(guidance: Option<&[String]>) -> String {
    const NONE: &str = "(none)";
    let entries = match guidance {
        Some(entries) if !entries.is_empty() => entries,
        _ => return NONE.to_string(),
    };
    let bullets: Vec<String> = entries
        .iter()
        .filter(|e| !e.trim().is_empty())
        .map(|e| {
            let flattened = e
                .split(['\n', '\r'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            format!("- {flattened}")
        })
        .collect();
    if bullets.is_empty() {
        return NONE.to_string();
    }
    bullets.join("\n")
}

/// Construct the repair prompt by substituting the verbatim validation error
/// into [`LEADER_REPAIR_PROMPT`].
pub fn build_repair_prompt(validation_error: &str) -> String {
    LEADER_REPAIR_PROMPT.replace("{{validation_error}}", validation_error)
}

pub const WORKFLOW_PREREQUISITES_PLACEHOLDER: &str = "{{workflow_prerequisites}}";

/// Empty summaries remove the complete placeholder line, including its newline.
pub fn substitute_workflow_prerequisites(prompt: &str, summary: &str) -> String {
    if summary.is_empty() {
        prompt
            .split_inclusive('\n')
            .filter(|line| !line.contains(WORKFLOW_PREREQUISITES_PLACEHOLDER))
            .collect()
    } else {
        prompt.replace(WORKFLOW_PREREQUISITES_PLACEHOLDER, summary)
    }
}
#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn leader_accepts_nonnumeric_identity_and_does_not_reinterpret_insertions() {
        let prompt = build_leader_prompt(
            "remote-1234567890abcdef12345678",
            "/workspace/{{available_agents}}.md",
            "claude",
            Some(2),
            None,
        );
        assert!(prompt.contains("work item remote-1234567890abcdef12345678"));
        assert!(prompt.contains("/workspace/{{available_agents}}.md"));
        assert!(!prompt.contains("{{work_item_identity}}"));
    }
}

#[cfg(test)]
mod image_recipe_prompt_tests {
    use super::*;

    fn prompt(recipes: Option<&ImageRecipePrompt>) -> String {
        build_squad_leader_prompt(
            "t",
            "describe {{image_recipes}} here",
            "/workspace",
            "- claude",
            &OverlayInventory::default(),
            "/awman/squad/run/verdict.json",
            None,
            recipes,
        )
    }

    #[test]
    fn directory_task_names_its_recipes_and_the_next_run_rule() {
        let recipes = ImageRecipePrompt::at(
            "/awman/context/workflow",
            &["claude".into(), "codex".into()],
            true,
        );
        let text = prompt(Some(&recipes));
        assert!(text.contains("`/awman/context/workflow/Dockerfile.dev`"));
        assert!(text.contains("`/awman/context/workflow/.awman/Dockerfile.claude`"));
        assert!(text.contains("`/awman/context/workflow/.awman/Dockerfile.codex`"));
        assert!(text.contains("at the start of the next run"));
        assert!(text.contains("every step of\nthe workflow you generate keep the images"));
        assert!(text.contains("visible immediately"));
        assert!(text.contains("are the same directory"));
        // The task's own text is data, never the section.
        assert!(text.contains("describe {{image_recipes}} here"));
        assert!(!text.contains("{{project_recipe}}") && !text.contains("{{agent_recipes}}"));
        assert!(!text.contains("{{same_directory_note}}"));

        let custom = prompt(Some(&ImageRecipePrompt::at(
            "/awman/context/workflow",
            &["claude".into()],
            false,
        )));
        assert!(custom.contains("Dockerfile.dev"));
        assert!(!custom.contains("are the same directory"));
    }

    #[test]
    fn git_task_prompt_has_no_recipe_section() {
        let text = prompt(None);
        assert!(!text.contains("image recipes"));
        assert!(!text.contains("Dockerfile.dev"));
        assert!(!text.contains("{{image_recipes}}\n"));
    }
}
