//! Embedded static assets for dynamic workflows (`exec workflow --dynamic`).
//!
//! These are compiled into the binary from `src/assets/dynamic/` and are
//! always regenerated from this embedded content at runtime — never read from
//! the host filesystem. See WI-0092.
//!
//! - [`EXAMPLE_WORKFLOW_TOML`] and [`WORKFLOW_USAGE_MD`] are written into the
//!   leader's workflow context directory as reference material.
//! - [`LEADER_PROMPT_MD`] is the leader prompt template; it is substituted in
//!   code (never written to disk) before being delivered to the leader agent.
//! - [`LEADER_REPAIR_PROMPT`] is the repair prompt template used when the
//!   leader's `workflow.toml` fails validation.

/// A complete example workflow shown to the leader agent as reference. Written
/// to `<context_dir>/example-workflow.toml`.
pub const EXAMPLE_WORKFLOW_TOML: &str = include_str!("../assets/dynamic/example-workflow.toml");

/// The complete workflow file-format documentation. Written to
/// `<context_dir>/workflow-usage.md`.
pub const WORKFLOW_USAGE_MD: &str = include_str!("../assets/dynamic/workflow-usage.md");

/// The leader prompt template. Substituted with `{{work_item_number}}`,
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

/// Construct the squad evaluation-leader prompt.
pub fn build_squad_leader_prompt(
    task_name: &str,
    task_description: &str,
    repo_mount_path: &str,
    available_agents: &str,
    overlays: &OverlayInventory,
    verdict_path: &str,
    guidance: Option<&[String]>,
) -> String {
    SQUAD_LEADER_PROMPT_MD
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
    work_item_number: &str,
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
    LEADER_PROMPT_MD
        .replace("{{work_item_number}}", work_item_number)
        .replace("{{work_item_path}}", work_item_path)
        .replace("{{available_agents}}", available_agents)
        .replace("{{max_concurrent_steps}}", &max_concurrent_steps)
        .replace("{{developer_guidance}}", &developer_guidance)
}

/// Render the repo config's `dynamicWorkflows.guidance` entries as the bullet
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
